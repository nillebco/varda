execution:
    1. plan (optional) --> requires human verification
    2. implement
    3. test plan
    4. test execution
        if this fails repeat from 2

---

Interactive sessions prompt the user for permissions --should we consider this as okay?

Chmod +x requires user interaction

Executing commands also
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 Bash command

   make -n install 2>&1 | head -20 && echo '---' && sh -n scripts/vclaude && sh -n scripts/vcodex && sh -n scripts/vcopilot && echo 'shell syntax ok'
   Dry-run make install and check script syntax

 Do you want to proceed?
 ❯ 1. Yes
   2. Yes, and don't ask again for similar commands in /Users/nilleb/dev/nillebco/varda
   3. No

And reading files also
 Read file

  Read(~/.varda/operations/tasks/users-nilleb-dev-nillebco-varda/fix-avoid-interactive-git-prompts.md)

 Do you want to proceed?
 ❯ 1. Yes
   2. Yes, allow reading from users-nilleb-dev-nillebco-varda/ during this session
   3. No
