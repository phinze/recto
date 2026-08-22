# recto

A jj-first terminal diff viewer. The right-hand page where the new
text lives, and where you read your agent's work.

This is a personal experiment. The terminal diff space is well-served
already (delta, difftastic, lumen, hunk, nvim-diffview are all good),
but none of them target the specific workflow I want: reviewing
changes that an AI agent just wrote in a sibling tmux pane, inside a
jj workspace, with the diff updating live as the agent saves. Recto
exists to be that tool, for me.

jj is primary, with a working Git fallback. Recto now has live reload,
revision and file navigation, search, syntax and word-level highlighting,
editor handoff, companion-agent focus and notes, and local GitHub review
drafts. Keybindings and shape will keep shifting as the tool finds itself.

## Status

In active use and still moving. See `CLAUDE.md` for the current architecture
and development conventions.

## License

MIT.
