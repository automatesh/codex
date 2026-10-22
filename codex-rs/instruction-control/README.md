# Accepted client instructions

This component freezes client instruction sources and installs accepted state behind Windows permissions. It does not change server policy or server model behavior.

The supported boundary is an ordinary, unelevated Codex process versus the policy manager explicitly elevated by the user. An already elevated process, malicious administrator, or compromised operating system is outside that boundary.

## Storage

The production location is the Windows machine `FOLDERID_ProgramFilesX64` known folder plus `CodexStripped`. Environment variables and user configuration cannot override it.

The application directory, all seven executables, binary digest manifest, and `Policy` subtree must have a trusted owner and no ACL granting ordinary principals write, delete, change-permissions, or change-owner access. Installation sets owner Administrators and a protected DACL: SYSTEM and Administrators full control; Users read and execute. Reparse points are rejected. The parent Program Files directory is checked too.

`Policy/active.json` is the accepted snapshot. Previous snapshots remain in `Policy/snapshots`. Source copies live in `Policy/sources/<snapshot-id>`; absolute original paths retain package layout under `tree/C/...` so accepted skill references and scripts continue to work. Binary package assets are encoded in candidate JSON and decoded into protected files during application. Skill metadata is materialized separately from its `SKILL.md` source. There is no live-file fallback.

At runtime the client verifies its protected executable location and its SHA-256 against the protected installation manifest. A modified executable in a user-writable checkout cannot become the trusted client by pointing at the protected policy directory.

## Candidate preparation

Preparation and inspection need no administrative rights and never accept instructions:

```powershell
.\codex-policy-manager.exe prepare --main .\main.md --catalog .\models.json --output .\candidate.json
.\codex-policy-manager.exe inspect --candidate .\candidate.json
```

An optional `--controls controls.json` accepts a `ForkConfig` object. Omitting it uses manual updates, rebuilt resume instructions, dangerous-command confirmation, local-only managed requirements, approved-only hooks, and **empty** Git commit trailer and PR footer.

Additional sources use `--source KIND=LOGICAL_ID=FILE`. For file packages, use the original absolute source file path as the logical ID. For example:

```powershell
.\codex-policy-manager.exe extend --candidate .\candidate.json --source 'skill=C:/skills/example/SKILL.md=C:/skills/example/SKILL.md' --source 'skill-asset=C:/skills/example/scripts/run.py=C:/skills/example/scripts/run.py' --output .\candidate-next.json
```

The main client also provides its own candidate collector so it can capture the complete effective source inventory. Unknown, changed, or newly supplied instructions must pass the client request gate before use.

## Initial installation

Keep these files together in the distribution:

- `codex.exe`
- `codex-policy-manager.exe`
- `codex-code-mode-host.exe`
- `codex-responses-api-proxy.exe`
- `codex-windows-sandbox-setup.exe`
- `codex-windows-sandbox-service.exe`
- `codex-command-runner.exe`

```powershell
.\codex-policy-manager.exe install --distribution . --candidate .\candidate.json
```

The manager requests Windows elevation with `runas`. The elevated manager then opens its own native review window, displaying the binary hashes and complete previous/candidate instruction content. Acceptance requires its **Accept this version** button; no `--yes` or command-line auto-approval exists. Cancelling changes no accepted state. Initial bootstrap requires trusting the downloaded installer and confirming the genuine Windows UAC prompt, as with other installers.

Distribution binaries remain open without write/delete sharing through review and copying. Candidate content is parsed into memory before review; those same bytes are applied, not a file reread afterward. No user-write interval is granted on the protected directory. Installation never starts Codex elevated.

Run the installed `codex.exe` from the protected installation afterward. Preserve the existing user `.codex` directory for sessions, authentication, and ordinary settings.

Run the installed manager's `verify` command without elevation after installation. It checks all seven executable hashes and accepted source copies, and requests mutating access to the application directory and active snapshot. Windows must deny those access requests. The check never writes, truncates, deletes, or creates files.

## Later instruction updates

```powershell
.\codex-policy-manager.exe apply --candidate .\candidate-next.json
```

An unelevated invocation verifies and launches the **installed protected manager**, which requests UAC and shows the exact old/new review. The manager archives the accepted snapshot, materializes protected source copies, and replaces `active.json` atomically. Concurrent changes during review cause an error requiring another review. Existing processes keep their pinned snapshot; restart Codex to load the newly accepted one.

The `install` command currently refuses an existing installation instead of partially replacing running binaries. Binary upgrades need a separate reviewed installation workflow; `apply` changes instruction snapshots only.

## Checks

The component tests cover source/control tampering, unknown fields, source collisions/traversal, binary assets, exact review contents, unsafe ordinary-user-owned storage, Windows argument quoting, and input file locks. They do not simulate or approve a real UAC prompt, and passing them is not evidence that installation was performed.
