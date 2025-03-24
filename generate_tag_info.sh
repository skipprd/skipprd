#!/bin/bash
# Get all tags in sorted order
tags=($(git tag | grep -v "^v" | grep -v "^test$" | sort -V))

# Generate diff and log for each tag
for i in "${!tags[@]}"; do
  current_tag="${tags[$i]}"
  echo "Processing ${current_tag}"
  # Get previous tag
  if [[ $i -eq 0 ]]; then
    # For the first tag, compare with the initial commit
    prev_commit=$(git rev-list --max-parents=0 HEAD)
    git diff $prev_commit $current_tag > "docs/releases/git/${current_tag}.diff"
    git log $prev_commit..$current_tag > "docs/releases/git/${current_tag}.log"
  else
    prev_tag="${tags[$i-1]}"
    git diff $prev_tag $current_tag > "docs/releases/git/${current_tag}.diff"
    git log $prev_tag..$current_tag > "docs/releases/git/${current_tag}.log"
  fi
  # Mark as done in todo.md
  sed -i.bak -E "s/- \\[ \\] ${current_tag}$/- [x] ${current_tag}/g" docs/releases/todo.md
  # Report progress
  echo "Generated files for ${current_tag}"
done
echo "All done!"
