##### Skippr Documentation

#### Configuration: CHAOS_MODE

#### Description
Enables a testing mode where random errors are intentionally thrown.

#### Default value
"no"

#### Example values

- "no" - Disables CHAOS_MODE. No random errors are thrown.
- "yes" - Enables CHAOS_MODE. Random errors are generated during data ingestion.

#### Detailed Description
CHAOS_MODE is a testing configuration that when enabled, introduces random errors into the data ingestion process. This can be useful for testing the robustness and reliability of your data pipeline, particularly when testing 'exactly once' semantics where each message must be delivered exactly once and not lost or duplicated.

When CHAOS_MODE is enabled (set to "yes"), the Skippr tool will periodically cause the running process to exit at random intervals between 15 and 60 seconds. This is intended to simulate various types of system failure, helping you to identify and troubleshoot potential issues with your data processing and recovery mechanisms.

When CHAOS_MODE is disabled (set to "no" or removed), Skippr will operate normally without introducing any artificial errors.

#### Considerations
Please use CHAOS_MODE carefully. This mode is designed for testing purposes and should not be used in production environments. Artificially induced errors can lead to data loss or duplication if your system does not handle such situations correctly.

Remember, the purpose of CHAOS_MODE is to help you identify potential issues in your data ingestion pipeline and improve its reliability. Use it as part of your testing and development process to build more robust data systems.

Note that the frequency of the induced errors when CHAOS_MODE is enabled is randomized, with intervals between 15 and 60 seconds. This variability is intended to simulate different types of system failure, providing a more comprehensive test of your error-handling capabilities.
