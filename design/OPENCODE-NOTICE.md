# OpenCode conversation presentation

AgentMux adapts the user-message panel, assistant Markdown presentation and
semantic syntax palette from OpenCode to Rust/ratatui. It retains AgentMux's
multi-agent routing, stream recovery and mouse interactions. Code-block borders
and width-aware wrapping are provided by the local ratatui adapter; OpenTUI's
TypeScript/native widgets are not imported.

Sources consulted on 2026-09-29 (upstream `dev`):

- https://github.com/anomalyco/opencode/blob/dev/packages/tui/src/routes/session/index.tsx
- https://github.com/anomalyco/opencode/blob/dev/packages/tui/src/context/theme.tsx
- https://github.com/anomalyco/opencode/blob/dev/packages/tui/src/theme/assets/opencode.json

The adapted presentation and palette retain the upstream license below.

MIT License

Copyright (c) 2025 opencode

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
