## 5.5.3

### Features
- Added new pipeline configurations for bike hire data processing
- Added support for very small files processing pipeline
- Added metrics pipeline with namespace field support
- Implemented new buffer indexing system with:
  - Column index offset tracking
  - Byte index offset tracking
  - Zone map functionality
  - Sparse indexing support
- Added new data input configurations for:
  - AWS CloudTrail logs
  - Bike hire data
  - Small file processing
  - Metric data with ordered depth and delimiter settings
- Added month and day partitioning parameters to SQL queries

### Technical Details
- Buffer thresholds configured for different pipeline types
- Added support for batch time fields and units
- Implemented namespace field support for metrics
- Added ordered depth and delimiter settings for S3 prefixes
- New buffer indexing system for improved data access patterns 

## 5.5.4

### Features
- Added retry mechanism for directory deletion operations
- Improved error handling for file system operations
- Added delay between retry attempts
- Enhanced error reporting for failed deletions

### Technical Details
- Modified `src/sql/query.rs` to:
  - Implement retry logic for directory deletion
  - Add 5-second delay between retry attempts
  - Set maximum retry count to 15 attempts
  - Improve error messages and logging
  - Handle known Rust issue with directory deletion

### Impact
These changes improve the reliability of file system operations, particularly when deleting directories. The retry mechanism helps handle temporary file system locks and race conditions, reducing the likelihood of failed deletions. The system will now attempt to delete directories multiple times before giving up, providing better resilience against temporary file system issues. 

## 5.5.5

### Features
- Enhanced RwLock handling to prevent deadlocks
- Added logging for lock wait times
- Implemented retry mechanism for lock acquisition
- Temporarily disabled heaptrack testing workflow

### Technical Details
- Modified `src/helpers/timed_rwlock.rs` to:
  - Add retry loop for lock acquisition with backoff
  - Implement logging for lock wait times
  - Add sleep intervals between lock attempts
  - Improve error handling for lock acquisition

- Modified `.github/workflows/build-publish.yml` to:
  - Comment out heaptrack testing workflow
  - Maintain other CI/CD functionality

### Impact
These changes improve system stability by preventing potential deadlocks in concurrent operations. The new lock management system provides better visibility into lock contention through logging and implements a more robust retry mechanism. The temporary removal of heaptrack testing does not affect the core functionality of the system. 

## 5.5.6

### Features
- Refactored configuration handling to improve code clarity and efficiency
- Simplified pipeline cache implementation
- Removed redundant configuration lookups
- Optimized string handling and memory usage

### Technical Details
- Modified `src/helpers/configuration.rs` to:
  - Remove unnecessary configuration lookups
  - Simplify pipeline configuration retrieval
  - Optimize string cloning operations
  - Clean up redundant code paths

- Modified `src/main.rs` to:
  - Refactor pipeline cache implementation
  - Improve pipeline synchronization logic
  - Optimize pipeline name handling
  - Remove redundant state management

### Impact
These changes improve code maintainability and performance by removing redundant operations and simplifying the codebase. The changes are focused on internal optimizations and should not affect the external behavior of the system. 