Create release notes.

cat the relevant git log and git diff file in ./docs/releases/git/[GIT_TAG_VERSION].[diff|log] to understand the nature of each change. Be specific to assist users in understanding the changes.

Save this changelog in ./docs/releases/[GIT_TAG_VERSION].md

where release notes already exist for a release, review them and make any improvements. They may be quite incorrect.

once you have processed a git tag, check it as done in ./docs/releases/todo.md