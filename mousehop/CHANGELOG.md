# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.14.1](https://github.com/jondkinney/mousehop/compare/v0.14.0...v0.14.1) - 2026-06-10

### Fixed

- *(tray)* match the menu-bar/tray glyph to the app-icon mark
- *(capture/macos)* poll display bounds to catch clamshell changes

## [0.14.0](https://github.com/jondkinney/mousehop/compare/v0.13.4...v0.14.0) - 2026-06-07

### Added

- *(scroll)* drop forwarded macOS momentum on non-mac sinks
- *(scroll)* [**breaking**] stop a forwarded coast on a finger rest

## [0.13.4](https://github.com/jondkinney/mousehop/compare/v0.13.3...v0.13.4) - 2026-06-06

### Fixed

- *(capture/macos)* recompute display bounds from scratch each refresh

## [0.13.3](https://github.com/jondkinney/mousehop/compare/v0.13.2...v0.13.3) - 2026-06-05

### Added

- *(input)* match host key-repeat settings for forwarded keys

## [0.13.2](https://github.com/jondkinney/mousehop/compare/v0.13.1...v0.13.2) - 2026-06-05

### Fixed

- *(macos)* document the menu-bar template flatten step in bundle metadata

## [0.13.1](https://github.com/jondkinney/mousehop/compare/v0.13.0...v0.13.1) - 2026-06-04

### Fixed

- *(macos)* bundle the icon, menu-bar template, and LSUIElement plist

### Other

- Merge pull request #46 from jondkinney/fix/macos-bundle-resources

## [0.13.0](https://github.com/jondkinney/mousehop/compare/v0.12.0...v0.13.0) - 2026-06-04

### Added

- *(macos)* MOUSEHOP_DISABLE_TCC_WATCH to keep the daemon alive without an AX grant
- `mousehop firewall` subcommand to open the LAN port
- dual-homed peer connection support

### Fixed

- *(firewall)* re-include windows in the `s` helper's cfg gate
- *(firewall)* gate Linux-only helpers to silence dead-code on other targets

### Other

- *(latency)* skip the refused-connect probe on windows

## [0.12.0](https://github.com/jondkinney/mousehop/compare/v0.11.8...v0.12.0) - 2026-05-28

### Added

- *(gtk)* record the release shortcut from preferences
- *(gtk)* raise the existing prefs window on a second launch

## [0.11.8](https://github.com/jondkinney/mousehop/compare/v0.11.7...v0.11.8) - 2026-05-26

### Added

- *(gtk)* About preferences group with copyable build info
- *(macos)* wake-aware listener teardown for DTLS reconnect
- *(network)* IPv6 dual-stack support

## [0.11.7](https://github.com/jondkinney/mousehop/compare/v0.11.6...v0.11.7) - 2026-05-22

### Other

- Offload config file write to the blocking pool
- Harden Windows capture against hook freeze and loss
- Fix macOS capture dying on lock and input lag

## [0.11.6](https://github.com/jondkinney/mousehop/compare/v0.11.5...v0.11.6) - 2026-05-22

### Fixed

- *(emulation)* refresh display bounds when monitors change

## [0.11.5](https://github.com/jondkinney/mousehop/compare/v0.11.4...v0.11.5) - 2026-05-21

### Added

- *(gtk)* single-instance guard for the preferences GUI

### Other

- rename mousehop-app/ -> mousehop/ for package/folder alignment

## [0.11.4](https://github.com/jondkinney/mousehop/compare/v0.11.3...v0.11.4) - 2026-05-20

### Other

- move root crate into mousehop-app/ subdirectory ([#17](https://github.com/jondkinney/mousehop/pull/17))
