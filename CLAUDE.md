@AGENTS.md

Claude-only note: skills for this repo are under `.claude/skills/` (symlinks
into `.agents/skills/`, plus the local `gpui-tomte` overlay). Load `gpui` and
`gpui-tomte` before touching any view, entity update, `cx.spawn`, layout, or
`#[gpui::test]`.
