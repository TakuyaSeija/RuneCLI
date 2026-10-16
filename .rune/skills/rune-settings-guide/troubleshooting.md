# Troubleshoot a Rune setting

1. Ask what the user wants to achieve and what actually happened. Note the installed version, exact key/menu and whether the problem is this session or every session.
2. Read the relevant topic and inspect only the necessary effective value with explain. Compare the factory default separately; a screenshot/custom value is not the default.
3. Validate a manually edited file with `rune -config check`. Fix the named type/range/unknown key without replacing unrelated settings. A valid file is not a successful network test.
4. Use `/logs` for reported failures and `/context` for what the AI received. Redact private work/account values before sharing either.
5. Propose the smallest relevant change, its scope/timing/risk and a reset route. Obtain the user's decision before applying or testing.

## Common distinctions
- Short terminal foreground wait moves work to background; output/log limits can stop it. Read the actual result.
- A folded UI box, a returned-text cap, and a model context overflow are different limits.
- Context window is not maximum reply length; reasoning also needs output room.
- Slow continuous streaming differs from no data arriving. Retry cannot fix invalid credentials or unsupported controls.
- Configured/key count does not prove an account works. Tests send real possibly billed requests.
- A missing pinned web provider is unavailable, not permission to choose another third party.
- Classifier enabled is not proof it is active; check mode/policy/status and reload explicitly when needed.
- No displayed thinking is not proof no reasoning occurred.
- A large spending warning threshold is not a hard budget cap.
- A list limit does not delete older sessions; compaction is not disk cleanup.

## After installing a new version
- On Linux and macOS, open windows and sessions move to the new version by themselves when idle; nothing needs closing. On Windows a running program cannot become another, so the new version is used the next time Rune is opened; open windows keep working on the version they started with.
- "Rune's data could not be updated for this version" means the new version could not take your data over. Nothing of yours was changed. Fix what the message names — usually free disk space or write access to the Rune data folder — and start Rune again. Open windows keep working on the version they run and try the new one again later.
- "left as it was (it could not be updated)" names one item, such as a session file Rune could not read. Everything else was updated; that item is unchanged.

Never solve a help problem by silently choosing Full, disabling review, wiping storage, reading credentials, installing software or changing a provider. Report what was actually verified and what remains uncertain.
