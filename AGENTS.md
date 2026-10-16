# aplexer

## Shell paths

- In Bash sessions, use forward slashes in paths, never backslashes. Use the path form supported by the active Bash environment, such as `/c/Users/...` or `C:/Users/...`.
- In PowerShell sessions, use full Windows paths with backslashes, such as `C:\Users\User\git\aplexer`.

## Commits

- Always make focused commits: one logical change per commit, staging only the files (or hunks) that belong to it. Never sweep unrelated working-tree changes into a commit.
