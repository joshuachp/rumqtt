use crate::notice::{self, NoticeTx};
use crate::{Event, Incoming, NoticeError, Outgoing, Request};

use crate::mqttbytes::v4::*;
use crate::mqttbytes::{self, *};
use std::collections::VecDeque;
use std::{io, time::Instant};

/// Errors during state handling
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// Io Error while state is passed to network
    #[error("Io error: {0:?}")]
    Io(#[from] io::Error),
    /// Invalid state for a given operation
    #[error("Invalid state for a given operation")]
    InvalidState,
    /// Received a packet (ack) which isn't asked for
    #[error("Received unsolicited ack pkid: {0}")]
    Unsolicited(u16),
    /// Last pingreq isn't acked
    #[error("Last pingreq isn't acked")]
    AwaitPingResp,
    /// Received a wrong packet while waiting for another packet
    #[error("Received a wrong packet while waiting for another packet")]
    WrongPacket,
    #[error("Timeout while waiting to resolve collision")]
    CollisionTimeout,
    #[error("A Subscribe packet must contain atleast one filter")]
    EmptySubscription,
    #[error("Mqtt serialization/deserialization error: {0}")]
    Deserialization(#[from] mqttbytes::Error),
    #[error("Connection closed by peer abruptly")]
    ConnectionAborted,
    #[error("Subscribe failed with reason '{reason:?}' ")]
    SubFail { reason: SubscribeReasonCode },
}

/// State of the mqtt connection.
// Design: Methods will just modify the state of the object without doing any network operations
// Design: All inflight queues are maintained in a pre initialized vec with index as packet id.
// This is done for 2 reasons
// Bad acks or out of order acks aren't O(n) causing cpu spikes
// Any missing acks from the broker are detected during the next recycled use of packet ids
#[derive(Debug)]
pub struct MqttState {
    /// Status of last ping
    pub await_pingresp: bool,
    /// Collision ping count. Collisions stop user requests
    /// which inturn trigger pings. Multiple pings without
    /// resolving collisions will result in error
    pub collision_ping_count: usize,
    /// Last incoming packet time
    last_incoming: Instant,
    /// Last outgoing packet time
    last_outgoing: Instant,
    /// Packet id of the last outgoing packet
    pub(crate) last_pkid: u16,
    /// Packet id of the last acked publish
    pub(crate) last_puback: u16,
    /// Number of outgoing inflight publishes
    pub(crate) inflight: u16,
    /// Maximum number of allowed inflight
    pub(crate) max_inflight: u16,
    /// Outgoing QoS 1, 2 publishes which aren't acked yet
    pub(crate) outgoing_pub1: VecDeque<(Publish, NoticeTx)>,
    pub(crate) outgoing_pub2: VecDeque<(Publish, NoticeTx)>,
    /// Packet ids of released QoS 2 publishes
    pub(crate) outgoing_rel: VecDeque<(u16, NoticeTx)>,
    /// Packet ids on incoming QoS 2 publishes
    pub(crate) incoming_pub: Vec<Option<u16>>,
    /// Outgoing subscribes
    pub(crate) outgoing_sub: VecDeque<(u16, NoticeTx)>,
    /// Outgoing unsubscribes
    pub(crate) outgoing_unsub: VecDeque<(u16, NoticeTx)>,

    /// Last collision due to broker not acking in order
    pub(crate) collision: Option<(Publish, NoticeTx)>,
    /// Buffered incoming packets
    pub events: VecDeque<Event>,
    /// Indicates if acknowledgements should be send immediately
    pub manual_acks: bool,
}

impl MqttState {
    /// Creates new mqtt state. Same state should be used during a
    /// connection for persistent sessions while new state should
    /// instantiated for clean sessions
    pub fn new(max_inflight: u16, manual_acks: bool) -> Self {
        MqttState {
            await_pingresp: false,
            collision_ping_count: 0,
            last_incoming: Instant::now(),
            last_outgoing: Instant::now(),
            last_pkid: 0,
            last_puback: 0,
            inflight: 0,
            max_inflight,
            // index 0 is wasted as 0 is not a valid packet id
            outgoing_pub1: VecDeque::with_capacity(10),
            outgoing_pub2: VecDeque::with_capacity(10),
            outgoing_rel: VecDeque::with_capacity(10),
            incoming_pub: vec![None; u16::MAX as usize + 1],
            outgoing_sub: VecDeque::with_capacity(10),
            outgoing_unsub: VecDeque::with_capacity(10),
            collision: None,
            // TODO: Optimize these sizes later
            events: VecDeque::with_capacity(100),
            manual_acks,
        }
    }

    /// Returns inflight outgoing packets and clears internal queues
    pub fn clean(&mut self) -> Vec<(NoticeTx, Request)> {
        let mut pending = Vec::with_capacity(100);

        for outgoing_pub in [&mut self.outgoing_pub1, &mut self.outgoing_pub2] {
            let mut second_half = Vec::with_capacity(100);
            let mut last_pkid_found = false;
            for (publish, tx) in outgoing_pub.drain(..) {
                let this_pkid = publish.pkid;
                let request = Request::Publish(publish);
                if !last_pkid_found {
                    second_half.push((tx, request));
                    if this_pkid == self.last_puback {
                        last_pkid_found = true;
                    }
                } else {
                    pending.push((tx, request));
                }
            }
            pending.extend(second_half);
        }
        println!("pending = {:?}", pending.len());

        // remove and collect pending releases
        for (pkid, tx) in self.outgoing_rel.drain(..) {
            let request = Request::PubRel(PubRel::new(pkid));
            pending.push((tx, request));
        }

        // remove packed ids of incoming qos2 publishes
        for id in self.incoming_pub.iter_mut() {
            id.take();
        }

        self.await_pingresp = false;
        self.collision_ping_count = 0;
        self.inflight = 0;
        pending
    }

    pub fn inflight(&self) -> u16 {
        self.inflight
    }

    /// Consolidates handling of all outgoing mqtt packet logic. Returns a packet which should
    /// be put on to the network by the eventloop
    pub fn handle_outgoing_packet(
        &mut self,
        tx: NoticeTx,
        request: Request,
    ) -> Result<Option<Packet>, StateError> {
        let packet = match request {
            Request::Publish(publish) => self.outgoing_publish(publish, tx)?,
            Request::PubRel(pubrel) => self.outgoing_pubrel(pubrel, tx)?,
            Request::Subscribe(subscribe) => self.outgoing_subscribe(subscribe, tx)?,
            Request::Unsubscribe(unsubscribe) => self.outgoing_unsubscribe(unsubscribe, tx)?,
            Request::PingReq(_) => self.outgoing_ping()?,
            Request::Disconnect(_) => self.outgoing_disconnect()?,
            Request::PubAck(puback) => self.outgoing_puback(puback)?,
            Request::PubRec(pubrec) => self.outgoing_pubrec(pubrec)?,
            _ => unimplemented!(),
        };

        self.last_outgoing = Instant::now();
        Ok(packet)
    }

    /// Consolidates handling of all incoming mqtt packets. Returns a `Notification` which for the
    /// user to consume and `Packet` which for the eventloop to put on the network
    /// E.g For incoming QoS1 publish packet, this method returns (Publish, Puback). Publish packet will
    /// be forwarded to user and Pubck packet will be written to network
    pub fn handle_incoming_packet(
        &mut self,
        packet: Incoming,
    ) -> Result<Option<Packet>, StateError> {
        let outgoing = match &packet {
            Incoming::PingResp => self.handle_incoming_pingresp()?,
            Incoming::Publish(publish) => self.handle_incoming_publish(publish)?,
            Incoming::SubAck(suback) => self.handle_incoming_suback(suback)?,
            Incoming::UnsubAck(unsuback) => self.handle_incoming_unsuback(unsuback)?,
            Incoming::PubAck(puback) => self.handle_incoming_puback(puback)?,
            Incoming::PubRec(pubrec) => self.handle_incoming_pubrec(pubrec)?,
            Incoming::PubRel(pubrel) => self.handle_incoming_pubrel(pubrel)?,
            Incoming::PubComp(pubcomp) => self.handle_incoming_pubcomp(pubcomp)?,
            _ => {
                error!("Invalid incoming packet = {:?}", packet);
                return Err(StateError::WrongPacket);
            }
        };
        self.events.push_back(Event::Incoming(packet));
        self.last_incoming = Instant::now();

        Ok(outgoing)
    }

    fn handle_incoming_suback(&mut self, suback: &SubAck) -> Result<Option<Packet>, StateError> {
        if suback.pkid > self.max_inflight {
            error!("Unsolicited suback packet: {:?}", suback.pkid);
            return Err(StateError::Unsolicited(suback.pkid));
        }
        // No expecting ordered acks for suback
        // Search outgoing_sub to find the right suback.pkid
        let pos = self
            .outgoing_sub
            .iter()
            .position(|(pkid, _)| *pkid == suback.pkid)
            .ok_or(StateError::Unsolicited(suback.pkid))?;
        let (_, tx) = self.outgoing_sub.remove(pos).unwrap();

        for reason in suback.return_codes.iter() {
            match reason {
                SubscribeReasonCode::Success(qos) => {
                    debug!("SubAck Pkid = {:?}, QoS = {:?}", suback.pkid, qos);
                }
                _ => {
                    tx.error(NoticeError::V4Subscribe(*reason));
                    return Err(StateError::SubFail { reason: *reason });
                }
            }
        }
        tx.success();

        Ok(None)
    }

    fn handle_incoming_unsuback(
        &mut self,
        unsuback: &UnsubAck,
    ) -> Result<Option<Packet>, StateError> {
        // No expecting ordered acks for unsuback
        // Search outgoing_unsub to find the right suback.pkid
        let notice = self
            .outgoing_unsub
            .iter()
            .position(|(pkid, _)| *pkid == unsuback.pkid)
            .and_then(|position| self.outgoing_unsub.swap_remove_back(position));

        match position {
            Some((_, tx)) => {
                tx.success();
            }
            None => {
                debug!("Unsolicited unsuback packet: {:?}", unsuback.pkid);
            }
        }

        Ok(None)
    }

    /// Results in a publish notification in all the QoS cases. Replys with an ack
    /// in case of QoS1 and Replys rec in case of QoS while also storing the message
    fn handle_incoming_publish(&mut self, publish: &Publish) -> Result<Option<Packet>, StateError> {
        let qos = publish.qos;

        match qos {
            QoS::AtMostOnce => Ok(None),
            QoS::AtLeastOnce => {
                if !self.manual_acks {
                    let puback = PubAck::new(publish.pkid);
                    return self.outgoing_puback(puback);
                }
                Ok(None)
            }
            QoS::ExactlyOnce => {
                let pkid = publish.pkid;
                self.incoming_pub[pkid as usize] = Some(pkid);

                if !self.manual_acks {
                    let pubrec = PubRec::new(pkid);
                    return self.outgoing_pubrec(pubrec);
                }
                Ok(None)
            }
        }
    }

    fn handle_incoming_puback(&mut self, puback: &PubAck) -> Result<Option<Packet>, StateError> {
        if puback.pkid > self.max_inflight {
            error!("Unsolicited puback packet: {:?}", puback.pkid);
            return Err(StateError::Unsolicited(puback.pkid));
        }
        // Expecting ordered acks for puback
        // Check front of outgoing_pub to see if it's in order
        let (_, tx) = self
            .outgoing_pub1
            .pop_front()
            .ok_or(StateError::Unsolicited(puback.pkid))?;
        tx.success();

        self.last_puback = puback.pkid;

        self.inflight -= 1;
        let packet = self.check_collision(puback.pkid).map(|(publish, tx)| {
            if publish.qos == QoS::AtLeastOnce {
                self.outgoing_pub1.push_back((publish.clone(), tx));
            } else if publish.qos == QoS::AtMostOnce {
                self.outgoing_pub2.push_back((publish.clone(), tx));
            }
            self.inflight += 1;

            let event = Event::Outgoing(Outgoing::Publish(publish.pkid));
            self.events.push_back(event);
            self.collision_ping_count = 0;

            Packet::Publish(publish)
        });

        Ok(packet)
    }

    fn handle_incoming_pubrec(&mut self, pubrec: &PubRec) -> Result<Option<Packet>, StateError> {
        if pubrec.pkid > self.max_inflight {
            error!("Unsolicited pubrec packet: {:?}", pubrec.pkid);
            return Err(StateError::Unsolicited(pubrec.pkid));
        }

        // Expecting ordered acks for pubrec
        // Check front of outgoing_pub to see if it's in order
        let (publish, tx) = self
            .outgoing_pub2
            .pop_front()
            .ok_or(StateError::Unsolicited(pubrec.pkid))?;
        if publish.pkid != pubrec.pkid {
            error!("Unsolicited pubrec packet: {:?}", pubrec.pkid);
            return Err(StateError::Unsolicited(pubrec.pkid));
        }

        // NOTE: Inflight - 1 for qos2 in comp
        self.outgoing_rel.push_back((pubrec.pkid, tx));
        let pubrel = PubRel { pkid: pubrec.pkid };
        let event = Event::Outgoing(Outgoing::PubRel(pubrec.pkid));
        self.events.push_back(event);

        Ok(Some(Packet::PubRel(pubrel)))
    }

    fn handle_incoming_pubrel(&mut self, pubrel: &PubRel) -> Result<Option<Packet>, StateError> {
        let publish = self
            .incoming_pub
            .get_mut(pubrel.pkid as usize)
            .ok_or(StateError::Unsolicited(pubrel.pkid))?;

        if publish.take().is_none() {
            error!("Unsolicited pubrel packet: {:?}", pubrel.pkid);
            return Err(StateError::Unsolicited(pubrel.pkid));
        }

        let event = Event::Outgoing(Outgoing::PubComp(pubrel.pkid));
        let pubcomp = PubComp { pkid: pubrel.pkid };
        self.events.push_back(event);

        Ok(Some(Packet::PubComp(pubcomp)))
    }

    fn handle_incoming_pubcomp(&mut self, pubcomp: &PubComp) -> Result<Option<Packet>, StateError> {
        if pubcomp.pkid > self.max_inflight {
            error!("Unsolicited pubcomp packet: {:?}", pubcomp.pkid);
            return Err(StateError::Unsolicited(pubcomp.pkid));
        }
        // Expecting ordered acks for pubcomp
        // Check front of outgoing_pub to see if it's in order
        let (pkid, tx) = self
            .outgoing_rel
            .pop_front()
            .ok_or(StateError::Unsolicited(pubcomp.pkid))?;
        if pkid != pubcomp.pkid {
            error!("Unsolicited pubcomp packet: {:?}", pubcomp.pkid);
            return Err(StateError::Unsolicited(pubcomp.pkid));
        }
        tx.success();

        self.inflight -= 1;
        let packet = self.check_collision(pubcomp.pkid).map(|(publish, tx)| {
            if publish.qos == QoS::AtLeastOnce {
                self.outgoing_pub1.push_back((publish.clone(), tx));
            } else if publish.qos == QoS::AtMostOnce {
                self.outgoing_pub2.push_back((publish.clone(), tx));
            }
            let event = Event::Outgoing(Outgoing::Publish(publish.pkid));
            self.events.push_back(event);
            self.collision_ping_count = 0;

            Packet::Publish(publish)
        });

        Ok(packet)
    }

    fn handle_incoming_pingresp(&mut self) -> Result<Option<Packet>, StateError> {
        self.await_pingresp = false;

        Ok(None)
    }

    /// Adds next packet identifier to QoS 1 and 2 publish packets and returns
    /// it buy wrapping publish in packet
    fn outgoing_publish(
        &mut self,
        mut publish: Publish,
        notice_tx: NoticeTx,
    ) -> Result<Option<Packet>, StateError> {
        if publish.qos != QoS::AtMostOnce {
            if publish.pkid == 0 {
                publish.pkid = self.next_pkid();
            }

            let pkid = publish.pkid;

            let outgoing_pub = match publish.qos {
                QoS::AtLeastOnce => &mut self.outgoing_pub1,
                QoS::ExactlyOnce => &mut self.outgoing_pub2,
                _ => unreachable!(),
            };
            if let Some(pos) = outgoing_pub
                .iter()
                .position(|(publish, _)| publish.pkid == pkid)
            {
                outgoing_pub.get(pos);
                info!("Collision on packet id = {:?}", publish.pkid);
                self.collision = Some((publish, notice_tx));
                let event = Event::Outgoing(Outgoing::AwaitAck(pkid));
                self.events.push_back(event);
                return Ok(None);
            }

            // if there is an existing publish at this pkid, this implies that broker hasn't acked this
            // packet yet. This error is possible only when broker isn't acking sequentially
            outgoing_pub.push_back((publish.clone(), notice_tx));
            self.inflight += 1;
        } else {
            notice_tx.success()
        }

        debug!(
            "Publish. Topic = {}, Pkid = {:?}, Payload Size = {:?}",
            publish.topic,
            publish.pkid,
            publish.payload.len()
        );

        let event = Event::Outgoing(Outgoing::Publish(publish.pkid));
        self.events.push_back(event);

        Ok(Some(Packet::Publish(publish)))
    }

    fn outgoing_pubrel(
        &mut self,
        pubrel: PubRel,
        notice_tx: NoticeTx,
    ) -> Result<Option<Packet>, StateError> {
        let pubrel = self.save_pubrel(pubrel, notice_tx)?;

        debug!("Pubrel. Pkid = {}", pubrel.pkid);
        let event = Event::Outgoing(Outgoing::PubRel(pubrel.pkid));
        self.events.push_back(event);

        Ok(Some(Packet::PubRel(pubrel)))
    }

    fn outgoing_puback(&mut self, puback: PubAck) -> Result<Option<Packet>, StateError> {
        let event = Event::Outgoing(Outgoing::PubAck(puback.pkid));
        self.events.push_back(event);

        Ok(Some(Packet::PubAck(puback)))
    }

    fn outgoing_pubrec(&mut self, pubrec: PubRec) -> Result<Option<Packet>, StateError> {
        let event = Event::Outgoing(Outgoing::PubRec(pubrec.pkid));
        self.events.push_back(event);

        Ok(Some(Packet::PubRec(pubrec)))
    }

    /// check when the last control packet/pingreq packet is received and return
    /// the status which tells if keep alive time has exceeded
    /// NOTE: status will be checked for zero keepalive times also
    fn outgoing_ping(&mut self) -> Result<Option<Packet>, StateError> {
        let elapsed_in = self.last_incoming.elapsed();
        let elapsed_out = self.last_outgoing.elapsed();

        if self.collision.is_some() {
            self.collision_ping_count += 1;
            if self.collision_ping_count >= 2 {
                return Err(StateError::CollisionTimeout);
            }
        }

        // raise error if last ping didn't receive ack
        if self.await_pingresp {
            return Err(StateError::AwaitPingResp);
        }

        self.await_pingresp = true;

        debug!(
            "Pingreq,
            last incoming packet before {} millisecs,
            last outgoing request before {} millisecs",
            elapsed_in.as_millis(),
            elapsed_out.as_millis()
        );

        let event = Event::Outgoing(Outgoing::PingReq);
        self.events.push_back(event);

        Ok(Some(Packet::PingReq))
    }

    fn outgoing_subscribe(
        &mut self,
        mut subscription: Subscribe,
        notice_tx: NoticeTx,
    ) -> Result<Option<Packet>, StateError> {
        if subscription.filters.is_empty() {
            return Err(StateError::EmptySubscription);
        }

        let pkid = self.next_pkid();
        subscription.pkid = pkid;

        debug!(
            "Subscribe. Topics = {:?}, Pkid = {:?}",
            subscription.filters, subscription.pkid
        );

        self.outgoing_sub.push_back((pkid, notice_tx));
        let event = Event::Outgoing(Outgoing::Subscribe(subscription.pkid));
        self.events.push_back(event);

        Ok(Some(Packet::Subscribe(subscription)))
    }

    fn outgoing_unsubscribe(
        &mut self,
        mut unsub: Unsubscribe,
        notice_tx: NoticeTx,
    ) -> Result<Option<Packet>, StateError> {
        let pkid = self.next_pkid();
        unsub.pkid = pkid;

        debug!(
            "Unsubscribe. Topics = {:?}, Pkid = {:?}",
            unsub.topics, unsub.pkid
        );

        self.outgoing_unsub.push_back((pkid, notice_tx));
        let event = Event::Outgoing(Outgoing::Unsubscribe(unsub.pkid));
        self.events.push_back(event);

        Ok(Some(Packet::Unsubscribe(unsub)))
    }

    fn outgoing_disconnect(&mut self) -> Result<Option<Packet>, StateError> {
        debug!("Disconnect");

        let event = Event::Outgoing(Outgoing::Disconnect);
        self.events.push_back(event);

        Ok(Some(Packet::Disconnect))
    }

    fn check_collision(&mut self, pkid: u16) -> Option<(Publish, NoticeTx)> {
        if let Some((publish, _)) = &self.collision {
            if publish.pkid == pkid {
                return self.collision.take();
            }
        }

        None
    }

    fn save_pubrel(
        &mut self,
        mut pubrel: PubRel,
        notice_tx: NoticeTx,
    ) -> Result<PubRel, StateError> {
        let pubrel = match pubrel.pkid {
            // consider PacketIdentifier(0) as uninitialized packets
            0 => {
                pubrel.pkid = self.next_pkid();
                pubrel
            }
            _ => pubrel,
        };

        self.outgoing_rel.push_back((pubrel.pkid, notice_tx));
        Ok(pubrel)
    }

    /// http://stackoverflow.com/questions/11115364/mqtt-messageid-practical-implementation
    /// Packet ids are incremented till maximum set inflight messages and reset to 1 after that.
    ///
    fn next_pkid(&mut self) -> u16 {
        let next_pkid = self.last_pkid + 1;

        // When next packet id is at the edge of inflight queue,
        // set await flag. This instructs eventloop to stop
        // processing requests until all the inflight publishes
        // are acked
        if next_pkid == self.max_inflight {
            self.last_pkid = 0;
            return next_pkid;
        }

        self.last_pkid = next_pkid;
        next_pkid
    }
}

#[cfg(test)]
mod test {
    use std::collections::VecDeque;

    use super::{MqttState, StateError};
    use crate::mqttbytes::v4::*;
    use crate::{mqttbytes::*, NoticeTx};
    use crate::{Event, Incoming, Outgoing, Request};

    fn build_outgoing_publish(qos: QoS) -> Publish {
        let topic = "hello/world".to_owned();
        let payload = vec![1, 2, 3];

        let mut publish = Publish::new(topic, QoS::AtLeastOnce, payload);
        publish.qos = qos;
        publish
    }

    fn build_incoming_publish(qos: QoS, pkid: u16) -> Publish {
        let topic = "hello/world".to_owned();
        let payload = vec![1, 2, 3];

        let mut publish = Publish::new(topic, QoS::AtLeastOnce, payload);
        publish.pkid = pkid;
        publish.qos = qos;
        publish
    }

    fn build_mqttstate() -> MqttState {
        MqttState::new(100, false)
    }

    #[test]
    fn next_pkid_increments_as_expected() {
        let mut mqtt = build_mqttstate();

        for i in 1..=100 {
            let pkid = mqtt.next_pkid();

            // loops between 0-99. % 100 == 0 implies border
            let expected = i % 100;
            if expected == 0 {
                break;
            }

            assert_eq!(expected, pkid);
        }
    }

    #[test]
    fn outgoing_publish_should_set_pkid_and_add_publish_to_queue() {
        let mut mqtt = build_mqttstate();

        // QoS0 Publish
        let publish = build_outgoing_publish(QoS::AtMostOnce);

        // QoS 0 publish shouldn't be saved in queue
        let (tx, _) = NoticeTx::new();
        mqtt.outgoing_publish(publish, tx).unwrap();
        assert_eq!(mqtt.last_pkid, 0);
        assert_eq!(mqtt.inflight, 0);

        // QoS1 Publish
        let publish = build_outgoing_publish(QoS::AtLeastOnce);

        // Packet id should be set and publish should be saved in queue
        let (tx, _) = NoticeTx::new();
        mqtt.outgoing_publish(publish.clone(), tx).unwrap();
        assert_eq!(mqtt.last_pkid, 1);
        assert_eq!(mqtt.inflight, 1);

        // Packet id should be incremented and publish should be saved in queue
        let (tx, _) = NoticeTx::new();
        mqtt.outgoing_publish(publish, tx).unwrap();
        assert_eq!(mqtt.last_pkid, 2);
        assert_eq!(mqtt.inflight, 2);

        // QoS1 Publish
        let publish = build_outgoing_publish(QoS::ExactlyOnce);

        // Packet id should be set and publish should be saved in queue
        let (tx, _) = NoticeTx::new();
        mqtt.outgoing_publish(publish.clone(), tx).unwrap();
        assert_eq!(mqtt.last_pkid, 3);
        assert_eq!(mqtt.inflight, 3);

        // Packet id should be incremented and publish should be saved in queue
        let (tx, _) = NoticeTx::new();
        mqtt.outgoing_publish(publish, tx).unwrap();
        assert_eq!(mqtt.last_pkid, 4);
        assert_eq!(mqtt.inflight, 4);
    }

    #[test]
    fn incoming_publish_should_be_added_to_queue_correctly() {
        let mut mqtt = build_mqttstate();

        // QoS0, 1, 2 Publishes
        let publish1 = build_incoming_publish(QoS::AtMostOnce, 1);
        let publish2 = build_incoming_publish(QoS::AtLeastOnce, 2);
        let publish3 = build_incoming_publish(QoS::ExactlyOnce, 3);

        mqtt.handle_incoming_publish(&publish1).unwrap();
        mqtt.handle_incoming_publish(&publish2).unwrap();
        mqtt.handle_incoming_publish(&publish3).unwrap();

        let pkid = mqtt.incoming_pub[3].unwrap();

        // only qos2 publish should be add to queue
        assert_eq!(pkid, 3);
    }

    #[test]
    fn incoming_publish_should_be_acked() {
        let mut mqtt = build_mqttstate();

        // QoS0, 1, 2 Publishes
        let publish1 = build_incoming_publish(QoS::AtMostOnce, 1);
        let publish2 = build_incoming_publish(QoS::AtLeastOnce, 2);
        let publish3 = build_incoming_publish(QoS::ExactlyOnce, 3);

        mqtt.handle_incoming_publish(&publish1).unwrap();
        mqtt.handle_incoming_publish(&publish2).unwrap();
        mqtt.handle_incoming_publish(&publish3).unwrap();

        if let Event::Outgoing(Outgoing::PubAck(pkid)) = mqtt.events[0] {
            assert_eq!(pkid, 2);
        } else {
            panic!("missing puback");
        }

        if let Event::Outgoing(Outgoing::PubRec(pkid)) = mqtt.events[1] {
            assert_eq!(pkid, 3);
        } else {
            panic!("missing PubRec");
        }
    }

    #[test]
    fn incoming_publish_should_not_be_acked_with_manual_acks() {
        let mut mqtt = build_mqttstate();
        mqtt.manual_acks = true;

        // QoS0, 1, 2 Publishes
        let publish1 = build_incoming_publish(QoS::AtMostOnce, 1);
        let publish2 = build_incoming_publish(QoS::AtLeastOnce, 2);
        let publish3 = build_incoming_publish(QoS::ExactlyOnce, 3);

        mqtt.handle_incoming_publish(&publish1).unwrap();
        mqtt.handle_incoming_publish(&publish2).unwrap();
        mqtt.handle_incoming_publish(&publish3).unwrap();

        let pkid = mqtt.incoming_pub[3].unwrap();
        assert_eq!(pkid, 3);

        assert!(mqtt.events.is_empty());
    }

    #[test]
    fn incoming_qos2_publish_should_send_rec_to_network_and_publish_to_user() {
        let mut mqtt = build_mqttstate();
        let publish = build_incoming_publish(QoS::ExactlyOnce, 1);

        let packet = mqtt.handle_incoming_publish(&publish).unwrap().unwrap();
        match packet {
            Packet::PubRec(pubrec) => assert_eq!(pubrec.pkid, 1),
            _ => panic!("Invalid network request: {:?}", packet),
        }
    }

    #[test]
    fn incoming_puback_should_remove_correct_publish_from_queue() {
        let mut mqtt = build_mqttstate();

        let publish1 = build_outgoing_publish(QoS::AtLeastOnce);
        let publish2 = build_outgoing_publish(QoS::ExactlyOnce);

        let (tx, _) = NoticeTx::new();
        mqtt.outgoing_publish(publish1, tx).unwrap();
        let (tx, _) = NoticeTx::new();
        mqtt.outgoing_publish(publish2, tx).unwrap();
        assert_eq!(mqtt.inflight, 2);

        mqtt.handle_incoming_puback(&PubAck::new(1)).unwrap();
        assert_eq!(mqtt.inflight, 1);

        mqtt.handle_incoming_pubrec(&PubRec::new(2)).unwrap();
        assert_eq!(mqtt.inflight, 1);

        let (tx, _) = NoticeTx::new();
        mqtt.outgoing_pubrel(PubRel::new(2), tx).unwrap();
        assert_eq!(mqtt.inflight, 1);

        mqtt.handle_incoming_pubcomp(&PubComp::new(2)).unwrap();
        assert_eq!(mqtt.inflight, 0);

        assert!(mqtt.outgoing_pub1.get(0).is_none());
        assert!(mqtt.outgoing_pub2.get(0).is_none());
    }

    #[test]
    fn incoming_puback_with_pkid_greater_than_max_inflight_should_be_handled_gracefully() {
        let mut mqtt = build_mqttstate();

        let got = mqtt.handle_incoming_puback(&PubAck::new(101)).unwrap_err();

        match got {
            StateError::Unsolicited(pkid) => assert_eq!(pkid, 101),
            e => panic!("Unexpected error: {}", e),
        }
    }

    #[test]
    fn incoming_pubrec_should_release_publish_from_queue_and_add_relid_to_rel_queue() {
        let mut mqtt = build_mqttstate();

        let publish1 = build_outgoing_publish(QoS::AtLeastOnce);
        let publish2 = build_outgoing_publish(QoS::ExactlyOnce);

        let (tx, _) = NoticeTx::new();
        let _publish_out = mqtt.outgoing_publish(publish1, tx);
        let (tx, _) = NoticeTx::new();
        let _publish_out = mqtt.outgoing_publish(publish2, tx);

        mqtt.handle_incoming_pubrec(&PubRec::new(2)).unwrap();
        assert_eq!(mqtt.inflight, 2);

        // check if the remaining element's pkid is 1
        let (backup, _) = mqtt.outgoing_pub1.get(0).unwrap();
        assert_eq!(backup.pkid, 1);

        // check if the qos2 element's release pkik has been set
        assert!(mqtt.outgoing_rel.get(0).is_some());
    }

    #[test]
    fn incoming_pubrec_should_send_release_to_network_and_nothing_to_user() {
        let mut mqtt = build_mqttstate();

        let publish = build_outgoing_publish(QoS::ExactlyOnce);
        let (tx, _) = NoticeTx::new();
        let packet = mqtt.outgoing_publish(publish, tx).unwrap().unwrap();
        match packet {
            Packet::Publish(publish) => assert_eq!(publish.pkid, 1),
            packet => panic!("Invalid network request: {:?}", packet),
        }

        let packet = mqtt
            .handle_incoming_pubrec(&PubRec::new(1))
            .unwrap()
            .unwrap();
        match packet {
            Packet::PubRel(pubrel) => assert_eq!(pubrel.pkid, 1),
            packet => panic!("Invalid network request: {:?}", packet),
        }
    }

    #[test]
    fn incoming_pubrel_should_send_comp_to_network_and_nothing_to_user() {
        let mut mqtt = build_mqttstate();
        let publish = build_incoming_publish(QoS::ExactlyOnce, 1);

        let packet = mqtt.handle_incoming_publish(&publish).unwrap().unwrap();
        match packet {
            Packet::PubRec(pubrec) => assert_eq!(pubrec.pkid, 1),
            packet => panic!("Invalid network request: {:?}", packet),
        }

        let packet = mqtt
            .handle_incoming_pubrel(&PubRel::new(1))
            .unwrap()
            .unwrap();
        match packet {
            Packet::PubComp(pubcomp) => assert_eq!(pubcomp.pkid, 1),
            packet => panic!("Invalid network request: {:?}", packet),
        }
    }

    #[test]
    fn incoming_pubcomp_should_release_correct_pkid_from_release_queue() {
        let mut mqtt = build_mqttstate();
        let publish = build_outgoing_publish(QoS::ExactlyOnce);

        let (tx, _) = NoticeTx::new();
        mqtt.outgoing_publish(publish, tx).unwrap();
        mqtt.handle_incoming_pubrec(&PubRec::new(1)).unwrap();

        mqtt.handle_incoming_pubcomp(&PubComp::new(1)).unwrap();
        assert_eq!(mqtt.inflight, 0);
    }

    #[test]
    fn outgoing_ping_handle_should_throw_errors_for_no_pingresp() {
        let mut mqtt = build_mqttstate();
        mqtt.outgoing_ping().unwrap();

        // network activity other than pingresp
        let publish = build_outgoing_publish(QoS::AtLeastOnce);
        let (tx, _) = NoticeTx::new();
        mqtt.handle_outgoing_packet(tx, Request::Publish(publish))
            .unwrap();
        mqtt.handle_incoming_packet(Incoming::PubAck(PubAck::new(1)))
            .unwrap();

        // should throw error because we didn't get pingresp for previous ping
        match mqtt.outgoing_ping() {
            Ok(_) => panic!("Should throw pingresp await error"),
            Err(StateError::AwaitPingResp) => (),
            Err(e) => panic!("Should throw pingresp await error. Error = {:?}", e),
        }
    }

    #[test]
    fn outgoing_ping_handle_should_succeed_if_pingresp_is_received() {
        let mut mqtt = build_mqttstate();

        // should ping
        mqtt.outgoing_ping().unwrap();
        mqtt.handle_incoming_packet(Incoming::PingResp).unwrap();

        // should ping
        mqtt.outgoing_ping().unwrap();
    }

    #[test]
    fn clean_is_calculating_pending_correctly() {
        let mut mqtt = build_mqttstate();

        fn build_outgoing_pub() -> VecDeque<(Publish, NoticeTx)> {
            let mut outgoing_pub = VecDeque::new();
            let (tx, _) = NoticeTx::new();
            outgoing_pub.push_back(
                // 2,
                (
                    Publish {
                        dup: false,
                        qos: QoS::AtMostOnce,
                        retain: false,
                        topic: "test".to_string(),
                        pkid: 1,
                        payload: "".into(),
                    },
                    tx,
                ),
            );
            let (tx, _) = NoticeTx::new();
            outgoing_pub.push_back(
                // 3,
                (
                    Publish {
                        dup: false,
                        qos: QoS::AtMostOnce,
                        retain: false,
                        topic: "test".to_string(),
                        pkid: 2,
                        payload: "".into(),
                    },
                    tx,
                ),
            );
            let (tx, _) = NoticeTx::new();
            outgoing_pub.push_back(
                // 4,
                (
                    Publish {
                        dup: false,
                        qos: QoS::AtMostOnce,
                        retain: false,
                        topic: "test".to_string(),
                        pkid: 3,
                        payload: "".into(),
                    },
                    tx,
                ),
            );
            let (tx, _) = NoticeTx::new();
            outgoing_pub.push_back(
                // 7,
                (
                    Publish {
                        dup: false,
                        qos: QoS::AtMostOnce,
                        retain: false,
                        topic: "test".to_string(),
                        pkid: 6,
                        payload: "".into(),
                    },
                    tx,
                ),
            );

            outgoing_pub
        }

        mqtt.outgoing_pub1 = build_outgoing_pub();
        mqtt.last_puback = 3;
        let requests = mqtt.clean();
        let res = vec![6, 1, 2, 3];
        for ((_, req), idx) in requests.iter().zip(res) {
            if let Request::Publish(publish) = req {
                assert_eq!(publish.pkid, idx);
            } else {
                unreachable!()
            }
        }

        mqtt.outgoing_pub1 = build_outgoing_pub();
        mqtt.last_puback = 0;
        let requests = mqtt.clean();
        let res = vec![1, 2, 3, 6];
        for ((_, req), idx) in requests.iter().zip(res) {
            if let Request::Publish(publish) = req {
                assert_eq!(publish.pkid, idx);
            } else {
                unreachable!()
            }
        }

        mqtt.outgoing_pub1 = build_outgoing_pub();
        mqtt.last_puback = 6;
        let requests = mqtt.clean();
        let res = vec![1, 2, 3, 6];
        for ((_, req), idx) in requests.iter().zip(res) {
            if let Request::Publish(publish) = req {
                assert_eq!(publish.pkid, idx);
            } else {
                unreachable!()
            }
        }
    }
}
