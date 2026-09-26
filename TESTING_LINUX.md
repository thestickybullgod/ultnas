# Testing Ultnas on Linux — Step by Step

This walks through installing the `.deb` and checking every major behavior,
with what you should see at each step. Allow about 45 minutes. Everything
except Part 9 runs as your normal user, in a throwaway folder, so nothing
important is touched.

You need a Debian or Ubuntu system (Ubuntu 22.04 or newer, Debian 12 or
newer) with systemd, and a terminal using UTF-8 (the default almost
everywhere).

---

## Part 0 — Get the package

1. On GitHub, open **Actions → Debian package**, click the newest green run,
   and download the **ultnas-deb** artifact. It is a zip containing
   `ultnas_0.1.0-1_amd64.deb`. Or, with the GitHub CLI:

   ```bash
   gh run download -R thestickybullgod/ultnas -n ultnas-deb
   ```

2. Unzip it if your browser didn't:

   ```bash
   unzip ultnas-deb.zip
   ```

## Part 1 — Install

```bash
sudo apt install ./ultnas_0.1.0-1_amd64.deb
```

**Expect:** the install finishes and prints "Ultnas is installed. To start
protecting your files:" followed by the setup commands. No service starts on
its own.

If apt also prints "N: Download is performed unsandboxed as root … couldn't be
accessed by user '_apt'", that's harmless: apt couldn't read your home folder
as its restricted user, so it read the file as root. To avoid it, install from
`/tmp` instead (`cp` the file there first).

Check the pieces are there:

```bash
ultnas --version          # ultnas 0.1.0
ultnasd --version         # ultnasd 0.1.0
man ultnas                # the manual page (q to quit)
```

Then check tab completion. Open a **new** terminal (completions load when a
shell starts), type `ultnas tr` without pressing Enter, and press the **Tab**
key twice.

**Expect:** the first Tab fills in `ultnas trac`; the second lists
`track  tracked`. If nothing happens, completion isn't switched on in your
shell: on bash, `sudo apt install bash-completion` and open a new terminal; on
zsh, add `autoload -U compinit && compinit` to `~/.zshrc`. This is only a
convenience: nothing later depends on it.

## Part 2 — A test helper

Paste this into your terminal. Every test uses it to show whether a file
contains invisible characters. It only reads.

```bash
check() {
  if grep -qP '[\x{200B}-\x{200F}\x{202A}-\x{202E}\x{2060}-\x{2064}\x{2066}-\x{2069}\x{FEFF}]' "$1"; then
    echo "DIRTY: $1 contains invisible characters"
  else
    echo "clean: $1"
  fi
}
```

## Part 3 — Set up and start

1. Make a sandbox with a few files:

   ```bash
   mkdir -p ~/ultnas-test/src
   printf 'is_admin = false\n'  > ~/ultnas-test/config.txt
   printf 'fn main() {}\n'      > ~/ultnas-test/src/main.rs
   printf 'hello\n'             > ~/ultnas-test/notes.txt
   ```

2. Create the vault by running setup. **Uncheck everything** for now: at
   the prompt, type `n` and press Enter. The list reprints with every box
   empty, followed by "→ 0 item(s) checked". Then press Enter again on the
   empty prompt to finish.

   ```bash
   ultnas setup
   ```

   **Expect:** "✓ Created vault … at /home/you/.local/share/ultnas", a checklist
   of what exists on your machine, then "Nothing selected." (If you'd rather
   also protect your real dotfiles now, leave them checked. Everything below
   still works.)

3. Track the sandbox, in its own namespace `test`:

   ```bash
   ultnas track --recursive ~/ultnas-test --namespace test
   ultnas tracked
   ```

   **Expect:** "3 text file(s) tracked", and `tracked` lists the directory and
   the three files, each with `status ok`.

4. Start the daemon and check on it:

   ```bash
   systemctl --user enable --now ultnasd
   systemctl --user status ultnasd      # active (running)
   ultnas daemon status
   ```

   **Expect:** `journal ok`, `quarantine none`,
   `watcher events — … director(y/ies) watched`, `tracking 3 file(s), 1 director(y/ies)`.

5. In a second terminal, keep the log open for the rest of the tests:

   ```bash
   journalctl --user -u ultnasd -f
   ```

## Part 4 — Invisible-character attacks

Each test writes into a file, then checks it one second later.

1. **Zero-width space.**

   ```bash
   printf 'is_\u200badmin = false\n' > ~/ultnas-test/config.txt; sleep 1; check ~/ultnas-test/config.txt
   ```

   **Expect:** `clean`. The log shows "1 invisible character(s) written to …
   (first: U+200B (zero-width) at 1:4)" and "stripped invisible characters".

2. **Bidi controls ("Trojan Source").**

   ```bash
   printf 'fn main() { /* \u202e } \u2066 */ }\n' > ~/ultnas-test/src/main.rs; sleep 1; check ~/ultnas-test/src/main.rs
   cat ~/ultnas-test/src/main.rs
   ```

   **Expect:** `clean`, and the file reads `fn main() { /*  }  */ }`: the
   visible text of that write stays, but it came with invisible characters,
   so it isn't trusted (next step).

3. **A write that carried invisible characters is held for approval.**

   ```bash
   ultnas tracked
   ```

   **Expect:** `main.rs` shows `status pending approval` even though the
   approval mode is automatic. Accept it, or put the original back:

   ```bash
   ultnas approve ~/ultnas-test/src/main.rs      # or: printf 'fn main() {}\n' > ~/ultnas-test/src/main.rs
   ```

4. **Pre-existing characters are part of the baseline.** Legitimate emoji
   sequences aren't touched either:

   ```bash
   printf 'family: 👨\u200d👩\u200d👧\n' >> ~/ultnas-test/notes.txt; sleep 1; cat ~/ultnas-test/notes.txt
   ```

   **Expect:** the family emoji is still intact (the zero-width joiners sit
   between emoji, where they belong), and `ultnas tracked` shows `notes.txt`
   `status ok`: it was accepted as an ordinary edit.

5. **An ordinary edit is accepted.**

   ```bash
   printf 'is_admin = false  # reviewed\n' > ~/ultnas-test/config.txt; sleep 1; ultnas tracked | grep -A1 config.txt
   ```

   **Expect:** `status ok`, with a new `stable` id.

## Part 5 — Repeated attacks: recreate, then quarantine

The defaults need 5 attempts for a recreate and 20 for a quarantine. To see
it quickly, use a test policy with low thresholds. The attacks below keep
the text you accepted in Part 4, adding only a zero-width space, so each is a
pure invisible-character attack.

1. Write the policy and point the vault at it:

   ```bash
   cat > ~/.local/share/ultnas/policy.toml <<'EOF'
   version = 1
   [global.integrity]
   write_violation_threshold = 2   # the 2nd attempt deletes and recreates
   escalate_after_restores   = 1   # after 1 restore, the next one quarantines
   [[namespaces]]
   path = "test"
   EOF
   ultnas policy validate ~/.local/share/ultnas/policy.toml
   echo 'policy_path = "policy.toml"' >> ~/.local/share/ultnas/vault.toml
   systemctl --user restart ultnasd
   ```

   **Expect:** "✓ Policy file is valid", and the log shows "policy loaded from
   …/policy.toml".

2. Attack four times:

   ```bash
   for i in 1 2 3 4; do printf 'is_\u200badmin = false  # reviewed\n' > ~/ultnas-test/config.txt; sleep 1; done
   ultnas daemon status
   ```

   **Expect:** under "Recent alerts": `sanitized` (attempt 1), `restored …
   from memory_cache` (attempt 2: the file was deleted and recreated),
   `sanitized` (attempt 3), then `quarantined test` (attempt 4). The status
   line reads `quarantine test`.

3. While quarantined, attacks are logged but left alone:

   ```bash
   printf 'is_\u200badmin = false  # reviewed\n' > ~/ultnas-test/config.txt; sleep 1; check ~/ultnas-test/config.txt
   ultnas integrity status
   ```

   **Expect:** `DIRTY`, and `integrity status` lists `test` under active
   quarantines.

4. Lift it. Protection resumes at once:

   ```bash
   ultnas integrity lift-quarantine test --yes
   printf 'is_admin = false  # reviewed\n' > ~/ultnas-test/config.txt; sleep 1
   printf 'is_\u200badmin = false  # reviewed\n' > ~/ultnas-test/config.txt; sleep 1; check ~/ultnas-test/config.txt
   ```

   **Expect:** "✓ Quarantine lift recorded", then `clean`.

## Part 6 — Approved mode

1. Require approval for the `test` namespace:

   ```bash
   printf 'approval = "approved"\n' >> ~/.local/share/ultnas/policy.toml
   systemctl --user restart ultnasd
   ```

2. Make an ordinary edit:

   ```bash
   printf 'draft\n' > ~/ultnas-test/notes.txt; sleep 1; ultnas tracked | grep -A2 notes.txt
   ```

   **Expect:** `status pending approval`, and the file still says `draft`:
   pending edits stay on disk.

3. Approve it:

   ```bash
   ultnas approve ~/ultnas-test/notes.txt; ultnas tracked | grep -A1 notes.txt
   ```

   **Expect:** "✓ Approved", then `status ok`.

4. Deletions inside a tracked directory are undone in approved mode:

   ```bash
   rm ~/ultnas-test/notes.txt; sleep 1; ls ~/ultnas-test/notes.txt
   ```

   **Expect:** the file is back. To really remove a file in approved mode,
   untrack it first: `ultnas untrack ~/ultnas-test/notes.txt`.

5. Switch back to automatic (remove the `approval` line), then restart:

   ```bash
   sed -i '/^approval/d' ~/.local/share/ultnas/policy.toml
   systemctl --user restart ultnasd
   ```

## Part 7 — Tracked directories

1. **New files are adopted, and cleaned first:**

   ```bash
   printf 'pay\u200bee = "bob"\n' > ~/ultnas-test/src/new.toml; sleep 1
   check ~/ultnas-test/src/new.toml; ultnas tracked | grep new.toml
   ```

   **Expect:** `clean`, and `new.toml` is listed.

2. **New subfolders are followed:**

   ```bash
   mkdir -p ~/ultnas-test/deep/er; printf 'x\n' > ~/ultnas-test/deep/er/f.txt; sleep 1
   printf 'x\u200b\n' > ~/ultnas-test/deep/er/f.txt; sleep 1; check ~/ultnas-test/deep/er/f.txt
   ```

   **Expect:** `clean`.

3. **Skipped:** hidden files, editor scratch files, excluded names:

   ```bash
   printf 'a\u200b\n' > ~/ultnas-test/.hidden; printf 'a\u200b\n' > ~/ultnas-test/x.swp; sleep 1
   check ~/ultnas-test/.hidden; check ~/ultnas-test/x.swp
   ```

   **Expect:** both `DIRTY`. They aren't tracked, by design.

4. **Deleting a file (automatic mode) stops tracking it:**

   ```bash
   rm ~/ultnas-test/src/new.toml; sleep 1; ultnas tracked | grep -c new.toml
   ```

   **Expect:** `0`, and the file stays deleted.

## Part 8 — Link swaps

Replace a tracked file with a symbolic link to something you can read, such
as `/etc/hostname`:

```bash
ln -sf /etc/hostname ~/ultnas-test/config.txt; sleep 1
ls -l ~/ultnas-test/config.txt; cat ~/ultnas-test/config.txt
```

**Expect:** `config.txt` is a regular file again (no `->`), holding your last
accepted `is_admin` line, and the log says it "was replaced by a link or other
non-regular file". The daemon never reads, writes, or copies the link's
target.

## Part 9 — System files (/etc), as root

This protects `/etc` with the system service. It's safe: nothing in `/etc`
changes unless something writes invisible characters into it.

1. Set up root's vault and choose `/etc`:

   ```bash
   sudo ultnas setup
   ```

   **Expect:** "✓ Created vault … at /var/lib/ultnas", and `/etc` in the list,
   checked. Uncheck anything else, and press Enter. It previews `/etc` ("it is
   outside your home directory") and asks: answer `y`. Files that already
   contain invisible characters are listed and left untracked.

2. Start the system service:

   ```bash
   sudo systemctl enable --now ultnasd
   sudo ultnas daemon status
   ```

3. Test with a throwaway file:

   ```bash
   echo 'a = 1' | sudo tee /etc/ultnas-test.conf >/dev/null; sleep 1
   printf 'a\u200b = 1\n' | sudo tee /etc/ultnas-test.conf >/dev/null; sleep 1
   check /etc/ultnas-test.conf
   ```

   **Expect:** `clean`. The system log (`sudo journalctl -u ultnasd`) shows the
   strip.

4. Clean up the test file:

   ```bash
   sudo rm /etc/ultnas-test.conf
   ```

   (In the default automatic mode, deleting is accepted. If you turned on
   approved mode for `/etc`, run `sudo ultnas untrack /etc/ultnas-test.conf`
   first, or it comes back.)

5. Check the guards: pseudo-filesystems can't be tracked.

   ```bash
   sudo ultnas track -r /proc
   ```

   **Expect:** an error, "/proc can't be tracked: /proc holds kernel or
   runtime state, not files".

## Part 10 — Status, logs, journal

```bash
ultnas daemon status              # live state
ultnas daemon status --json       # the same, for scripts
ultnas integrity status           # counts from the journal
ultnas integrity violations       # recent events
journalctl --user -u ultnasd --since "1 hour ago"
tail -n 5 ~/.local/share/ultnas/journal.log
```

**Expect:** the events from the tests above in each. The journal rotates past
64 MiB (`journal.log.1` …) and carries quarantines over.

## Part 11 — Optional: mirror and retention

1. Rewrite the test policy with a mirror (a second copy of everything,
   ideally on another disk) and retention:

   ```bash
   cat > ~/.local/share/ultnas/policy.toml <<EOF
   version = 1
   [global.integrity]
   mirror = "$HOME/ultnas-mirror"
   [[namespaces]]
   path = "test"
     [namespaces.retention]
     keep_versions = 2
   EOF
   ultnas policy validate ~/.local/share/ultnas/policy.toml
   systemctl --user restart ultnasd; sleep 2; ls ~/ultnas-mirror/objects | head
   ```

   **Expect:** object folders appear, and the log warns that the mirror shares
   the vault's filesystem (fine for a test).

2. Preview which old versions retention would purge:

   ```bash
   ultnas purge --rotate --dry-run
   ```

   **Expect:** at least one older version (for example of `notes.txt`, which
   has had several) "beyond the newest 2 version(s)", then "(dry run: nothing
   purged)". Current versions are never listed. The daemon applies the same
   rules hourly.

## Part 12 — Robustness

1. **One daemon per vault:**

   ```bash
   ultnasd
   ```

   **Expect:** an error, "vault … is already in use".

2. **Stop and restart:**

   ```bash
   ultnas daemon stop; sleep 1; systemctl --user status ultnasd | head -3
   systemctl --user start ultnasd
   ```

   **Expect:** it stops cleanly (systemd shows it inactive, since a clean
   exit isn't a failure), then starts again.

3. **Changes while stopped are caught on start:**

   ```bash
   systemctl --user stop ultnasd
   printf 'is_\u200badmin = false  # reviewed\n' > ~/ultnas-test/config.txt
   systemctl --user start ultnasd; sleep 2; check ~/ultnas-test/config.txt
   ```

   **Expect:** `clean`.

4. **After a reboot:** user services run while you're logged in. For
   protection before you log in, run `loginctl enable-linger $USER`. The
   system service (Part 9) always runs.

## Part 13 — Uninstall

```bash
systemctl --user disable --now ultnasd
sudo systemctl disable --now ultnasd     # if you did Part 9
sudo apt remove ultnas
```

**Expect:** the commands are gone (`command -v ultnas` prints nothing). The
vaults are kept on purpose, since they hold your file history. Delete them
yourself if you want:

```bash
rm -rf ~/.local/share/ultnas ~/ultnas-mirror ~/ultnas-test
sudo rm -rf /var/lib/ultnas
```

---

## Reporting a problem

Please include:

- the step that failed, what you expected, and what you saw
- `ultnas daemon status --json`
- `journalctl --user -u ultnasd --since "30 minutes ago"` (or
  `sudo journalctl -u ultnasd …` for the system service)
- the last lines of `~/.local/share/ultnas/journal.log`
- your distribution and version (`cat /etc/os-release`)
