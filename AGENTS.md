# Agent Instructions

Do NOT run `git add`, `git commit`, or any other git history-modifying command. Varda owns committing for any task it drives.

When you edit, create, delete, format, or otherwise change tracked project files:

1. Run the relevant verification for the change when practical.
2. Leave the changes in the working tree, unstaged.
3. List every changed file (one absolute path per line) under the `Files touched` heading of your recap. Varda stages and commits exactly those paths after the run.

If unrelated user changes are already present, do not revert them and do not list them under `Files touched` unless they are required for the current change.

Think if a README update could make sense.
