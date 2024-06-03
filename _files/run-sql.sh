#!/bin/bash

# List of pipeline names passed as arguments to the script
PIPELINE_NAMES=("$@")

# Loop through each pipeline name and execute the command
for PIPELINE_NAME in "${PIPELINE_NAMES[@]}"; do
#    echo "Disabling pipeline: $PIPELINE_NAME"
#    SKIPPR_PROFILE=skippr-dev cargo run query --sql "disable PIPELINE $PIPELINE_NAME"
#    SKIPPR_PROFILE=skippr-dev cargo run query --sql "reset PIPELINE $PIPELINE_NAME"
#    SKIPPR_PROFILE=skippr-dev cargo run query --sql "drop pipeline $PIPELINE_NAME"
#    SKIPPR_PROFILE=skippr-dev cargo run query --sql "drop schema $PIPELINE_NAME.recharge"
#    SKIPPR_PROFILE=skippr-dev cargo run query --sql "drop schema $PIPELINE_NAME.maintenance"
#    SKIPPR_PROFILE=skippr-dev cargo run query --sql "drop schema $PIPELINE_NAME.trip"

    SKIPPR_PROFILE=skippr-dev cargo run query --sql "ENABLE PIPELINE $PIPELINE_NAME"
done
