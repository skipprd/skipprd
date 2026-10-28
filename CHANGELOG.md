# Changelog

All notable changes to this project will be documented in this file.

## [15.12.1]

### Fixed

- Runtime plugin discovery uses `{skipprd-semver}/manifest-index.json`, not `/latest/`. Tagged 15.12.1 hosts stay on the protocol-17 plugin bundle published with this tag.
- Release CI matches current rust.yml: GitHub-hosted runners and Cloudflare R2. No Depot, ECR, CodeArtifact, or AWS S3.

## [5.7.2] - 2024-03-23

### Added
- Full support for `array` of `structs` data type
- New syntax support in configuration files
- Schema dump now outputs metadata structure for easier alter statements
- Automatic schema synchronization on startup

### Changed
- Improved CPU thread count handling
- Updated output plugin naming convention
- Code cleanup and optimization

### Fixed
- Various compilation issues
- Syntax-related bugs
- Performance issues with large record sets

### Technical Details
- Schema synchronization improvements
- Code structure optimization
- Build system enhancements

## [0.1.0] - 2024-03-23
- Initial development version 