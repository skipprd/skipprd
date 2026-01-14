
This repository is a Cargo workspace with two crates:

- `skippr`: ingest + plugins + `sqlrt`
- `react`: ReAct runtime + WebSocket server (`serve`)

To run the ReAct server:

```bash
cargo run -p react -- serve --port 8787 --log
```


Usage: skippr convert <INPUT> <OUTPUT> [OPTIONS]

Arguments:
  <INPUT>   Input format
  <OUTPUT>  Output format

Options:
  -i, --input
  -o, --output
  -x, --xyz 
  		Any options defined in the INPUT and OUTPUT serdes



skippr init


skippr discover DATA_SOURCE


skippr sync DATA_SOURCE DATA_DEST
	--schema 
	--data 

Global options:
- `--log` Enable diagnostic logs (disabled by default). Respect `RUST_LOG` for level (e.g., `RUST_LOG=debug`).


skippr validate DATA_SOURCE
	--schema-version		- Validate source data against schema version (defaults to latest approved schema)


skippr diff DATA_SOURCE
	--schema-version 		- Schema version to compare (defaults to latest approved schema)
	--previous-version  	- Schema version to generate diff against



skippr destroy DATA_SOURCE



Schemas

Ingest

Catalog

Liniage

Query