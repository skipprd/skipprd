#!/bin/bash
# Function to verify if a tag follows semantic versioning
verify_semver() {
  local tag=$1
  if [[ $tag =~ ^[0-9]+\.[0-9]+\.[0-9]+(-alpha)?$ ]]; then
    echo "correct"
  else
    echo "incorrect"
  fi
}

# Read todo.md line by line
while IFS= read -r line; do
  # Check if the line contains a tag
  if [[ $line =~ ^-\ \[.\]\ ([0-9]+\.[0-9]+\.[0-9]+(-alpha)?) ]]; then
    tag="${BASH_REMATCH[1]}"
    # Log file path
    log_file="docs/releases/git/${tag}.log"
    if [[ -f "$log_file" ]]; then
      # Get the date of the last commit
      date=$(grep -m 1 "Date:" "$log_file" | sed 's/Date:\s*//')
      # Verify semver
      semver_status=$(verify_semver "$tag")
      # Replace the line in todo.md
      if [[ -n "$date" ]]; then
        # Remove any existing comment
        clean_line=$(echo "$line" | sed 's/ #.*//')
        # Escape special characters in the line and date
        escaped_line=$(echo "$line" | sed 's/[\/&]/\\&/g')
        replacement="${clean_line} # Last commit: ${date}, SemVer: ${semver_status}"
        escaped_replacement=$(echo "$replacement" | sed 's/[\/&]/\\&/g')
        # Use a different delimiter to avoid issues with slashes in the date
        sed -i.bak "s|${escaped_line}|${escaped_replacement}|" docs/releases/todo.md
        echo "Updated $tag with date: $date"
      else
        echo "No date found for $tag"
      fi
    else
      echo "Log file not found for $tag"
    fi
  fi
done < docs/releases/todo.md

# Clean up backup files
find . -name "*.bak" -type f -delete

echo "Todo.md updated with commit dates and semver validation"
