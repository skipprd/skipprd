#!/bin/bash

# Function to recursively count objects in a prefix
function count_objects {
    local current_prefix=$1
    # Count objects for the current prefix
    local count=$(aws s3api list-objects --bucket "$bucket" --prefix "$current_prefix" --output json | jq '.Contents | length')
    echo "$current_prefix $count"

    # Find sub-prefixes and recurse
    local sub_prefixes=$(aws s3api list-objects --bucket "$bucket" --prefix "$current_prefix" --delimiter '/' --output text --query 'CommonPrefixes[].Prefix')
    for sub_prefix in $sub_prefixes; do
        count_objects "$sub_prefix"
    done
}

# Replace these with your actual bucket name and root prefix
bucket="skippr-dev-datalake"
root_prefix="small_files_7/"
temp_file=$(mktemp)

# Check if root_prefix ends with a slash and add one if it doesn't
[[ "$root_prefix" != */ ]] && root_prefix="$root_prefix/"

# Temp file for storing results before sorting
temp_file=$(mktemp)

# Start the recursive counting
count_objects "$root_prefix" > "$temp_file"

# Sort the results by count in descending order and display
sort -k2,2nr "$temp_file"

# Clean up
rm "$temp_file"