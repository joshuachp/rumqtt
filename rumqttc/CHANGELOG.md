# CHANGELOG

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
 - Added support for MQTT5 features to v5 client
   - Refactored v5::mqttbytes to use associated functions & include properties
   - Added new API's on v5 client for properties, eg `publish_with_props` etc
   - Refactored `MqttOptions` to use `ConnectProperties` for some fields
   - Other minor changes for MQTT5

### Changed
- Remove `Box` on `Event::Incoming`

### Deprecated

### Removed
 - Removed dependency on pollster

### Fixed
 - Fixed v5::mqttbytes `Connect` packet returning wrong size on `write()`
   - Added tests for packet length for all v5 packets

### Security

---

## [rumqttc 0.20.0] - 17-01-2023

### Added
- `NetworkOptions` added to provide a way to configure low level network configurations (#545)

### Changed
- `options` in `Eventloop` now is called `mqtt_options` (#545)
- `ConnectionError` now has specific variant for type of `Timeout`, `FlushTimeout` and `NetworkTimeout` instead of generic `Timeout` for both (#545)
- `conn_timeout` is moved into `NetworkOptions` (#545)

---

Old changelog entries can be found at [CHANGELOG.md](../CHANGELOG.md)