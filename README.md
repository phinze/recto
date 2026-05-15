# recto

A jj-first terminal diff viewer. The right-hand page where the new
text lives, and where you read your agent's work.

This is a personal experiment. The terminal diff space is well-served
already (delta, difftastic, lumen, hunk, nvim-diffview are all good),
but none of them target the specific workflow I want: reviewing
changes that an AI agent just wrote in a sibling tmux pane, inside a
jj workspace, with the diff updating live as the agent saves. Recto
exists to be that tool, for me.

jj is primary; git fallback is planned. Keybindings and shape will
shift as the tool finds itself.

## Status

v0, in motion. See `CLAUDE.md` for the architecture sketch and what's
landed.

## License

MIT.
