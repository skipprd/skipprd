##### Skippr Configuration: DATA_DIR

#### Config Name
DATA_DIR

#### Description
Specifies the directory path where Skippr stores its flushed buffer files and offsets database.

#### Default Value
If not explicitly set, the default value for `DATA_DIR` is `./data`.

#### Example Values

- `DATA_DIR=./buffer` : Skippr will store its flushed buffer files and offsets database in the `buffer` directory located at the root level of the application.

- `DATA_DIR=/home/user/skippr/data` : In this case, Skippr will use the directory at the absolute path `/home/user/skippr/data`.

- `DATA_DIR=../data/skippr` : If a relative path is given, Skippr will resolve it based on the current working directory of the process. Here, it will go one level up from the current directory and then go into `data/skippr`.

#### Detailed Description
The `DATA_DIR` configuration parameter determines the directory where Skippr persists its buffer files and offsets database. These files play a crucial role in data ingestion and recovery procedures. When data is ingested by Skippr, it first lands in a buffer. After some processing, the buffer data is flushed to a file in the `DATA_DIR`. The offsets database, which maintains the state of data ingestion, is also stored here.

The `DATA_DIR` parameter accepts both absolute and relative paths. If a relative path is provided, it will be resolved based on the current working directory of the Skippr process.

When `DATA_DIR` is set, Skippr attempts to create the directory (including any necessary but nonexistent parent directories) at the specified path. If the creation fails, Skippr will terminate with an error message.

#### Considerations

- Ensure that the Skippr process has the necessary read and write permissions for the directory specified by `DATA_DIR`. Failure to do so may lead to unexpected errors or data loss.

- Be mindful of the storage capacity of the drive where the `DATA_DIR` is located. As data is ingested, Skippr will persistently write to this directory, which could lead to increased storage usage over time.

- If a relative path is used, remember that it is resolved from the current working directory of the Skippr process, which might be different from the directory where the Skippr executable or script resides.

- For resilience, consider placing the `DATA_DIR` on a drive that is regularly backed up, allowing recovery of buffer files and offsets in case of system failures.

- If you change the `DATA_DIR` after Skippr has started ingesting data, previously ingested data will not be automatically moved or copied to the new location. You must handle such data migrations manually.
