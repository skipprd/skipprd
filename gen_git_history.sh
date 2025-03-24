#!/bin/bash

# Create output directory
mkdir -p ./docs/releases/git

# Get tags from todo.md and sort them
TAGS=$(grep -o "\- \[x\] [0-9]\+\.[0-9]\+\.[0-9]\+.*" docs/releases/todo.md | sed 's/- \[x\] //' | sed 's/ #.*//' | sort -V)

# Store tags in an array
TAGS_ARRAY=()
while IFS= read -r line; do
  TAGS_ARRAY+=("$line")
done <<< "$TAGS"

# Loop through tags (starting from the second tag)
for ((i=1; i<${#TAGS_ARRAY[@]}; i++)); do
  CURRENT_TAG="${TAGS_ARRAY[$i]}"
  PREVIOUS_TAG="${TAGS_ARRAY[$i-1]}"
  
  echo "Processing $CURRENT_TAG (compared to $PREVIOUS_TAG)..."
  
  # Create git diff file
  git diff $PREVIOUS_TAG $CURRENT_TAG > "./docs/releases/git/${CURRENT_TAG}.diff"
  
  # Create git log file
  git log $PREVIOUS_TAG..$CURRENT_TAG > "./docs/releases/git/${CURRENT_TAG}.log"
done

echo "Done! Files have been created in ./docs/releases/git/" 