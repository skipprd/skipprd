
This repository is a Cargo workspace with one crate:


- `skippr`: ingest + plugins + `sqlrt`


Usage: skipprd convert <INPUT> <OUTPUT> [OPTIONS]

Arguments:
  <INPUT>   Input format
  <OUTPUT>  Output format

Options:
  -i, --input
  -o, --output
  -x, --xyz 
  		Any options defined in the INPUT and OUTPUT serdes



skipprd init


skipprd discover DATA_SOURCE


skipprd sync DATA_SOURCE DATA_DEST
	--schema 
	--data 

Global options:
- `--log` Enable diagnostic logs (disabled by default). Respect `RUST_LOG` for level (e.g., `RUST_LOG=debug`).


skipprd validate DATA_SOURCE
	--schema-version		- Validate source data against schema version (defaults to latest approved schema)


skipprd diff DATA_SOURCE
	--schema-version 		- Schema version to compare (defaults to latest approved schema)
	--previous-version  	- Schema version to generate diff against



skipprd destroy DATA_SOURCE



Schemas

Ingest

Catalog

Liniage

Query
