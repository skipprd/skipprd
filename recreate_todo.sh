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

# Output file
output_file="docs/releases/todo.md.new"

# Write header
echo "# Release Notes Todo List" > $output_file
echo "" >> $output_file
echo "Legend:" >> $output_file
echo "- [ ] Release notes exist" >> $output_file
echo "- [ ] Release notes pending " >> $output_file
echo "" >> $output_file

# Process each section
process_section() {
  local section=$1
  local pattern=$2
  
  echo "## $section Series" >> $output_file
  
  # Find all tags matching the pattern
  for tag in $(git tag | grep -v "^v" | grep -v "^test$" | grep "^$pattern" | sort -V); do
    log_file="docs/releases/git/${tag}.log"
    
    # Check if the tag exists in todo.md (to maintain the [x] status)
    if grep -q "- \[x\] $tag" docs/releases/todo.md; then
      status="x"
    else
      status=" "
    fi
    
    # Get commit date
    if [[ -f "$log_file" ]]; then
      date=$(grep -m 1 "Date:" "$log_file" | sed 's/Date:\s*//')
      
      # Verify semver
      semver_status=$(verify_semver "$tag")
      
      if [[ -n "$date" ]]; then
        echo "- [$status] $tag # Last commit: $date, SemVer: $semver_status" >> $output_file
      else
        echo "- [$status] $tag # No date found, SemVer: $semver_status" >> $output_file
      fi
    else
      echo "- [$status] $tag # No log file found" >> $output_file
    fi
  done
  
  echo "" >> $output_file
}

# Process each major version
process_section "0.x.x" "0\."
process_section "1.x.x" "1\."
process_section "3.x.x" "3\."
process_section "4.x.x" "4\."
process_section "5.x.x" "5\."

# Replace the old file
mv $output_file docs/releases/todo.md

echo "Todo.md recreated with commit dates and semver validation" 