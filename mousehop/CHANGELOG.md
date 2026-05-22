# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
