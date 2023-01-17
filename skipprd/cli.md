

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