# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.11.2] - 2026-05-19

### Fixed

- Flatpak icon validation: GdkPixbuf's SVG sniffer reads only the first
  ~256 bytes looking for the `<svg` tag, and the multi-line XML
  docstring above the root element pushed it past that window. Moved
  the docstring inside `<svg>` so Flatpak's icon validator accepts the
  app icon and the Flatpak bundle exports cleanly.

## [0.11.1] - 2026-05-19

### Fixed

- Typo "occured" → "occurred" in the `IpcError::Io` thiserror message
  (user-visible in logs and CLI output), a doc comment on
  `FrontendEvent::Error`, and the `capture_event_occured` local
  variable in `input-capture`'s libei capture loop.
