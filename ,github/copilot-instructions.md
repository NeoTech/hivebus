> Lean routing document. Detailed procedures live in **skills** and **docs**.

## Agent Rules

- **Never open a new terminal** if there is already an agent-controlled terminal open. Reuse the existing terminal.
- **EADDRINUSE error**: Inform the user the server is already running. Do **not** kill the process.
- **Allowed commands** in agent-controlled terminal: `bun`, `bunx`, `bun run dev`, `bun run build`, `bun run test`, `bun run lint`, `bun run e2e`, `kill <pid>`, `bun kill-port <port>`, `ls`, `cat <file>`, `tail -f <file>`, `diff <file1> <file2>`, `echo <message>`, `exit`, `clear`, `cd <dir>`, `pwd`, `rm <file>`, `mkdir <dir>`, `mv <source> <destination>`, `cp <source> <destination>`, `bun install <package>`, `bun uninstall <package>`, `bun update <package>`. `cargo`, `rustup`, `rustc`, `wasm-pack`, `cargo install`, `cargo uninstall`, `cargo update`, `cargo build`, `cargo test`, `cargo run`, `cargo check`, `cargo clippy`, `cargo fmt`, `cargo doc`, `cargo clean`, `cargo tree`, `cargo search <crate>`, `cargo publish`, `cargo login`, `cargo logout`, `cargo version`, `cargo help`, `cargo new <project>`, `cargo init`, `cargo add <dependency>`, `cargo remove <dependency>`, `cargo update <dependency>`, `cargo vendor`, `cargo generate-lockfile`, `cargo metadata`, `cargo fix`, `cargo bisect`, `cargo doc --open`, `cargo clippy --fix`, `cargo fmt --check`, `cargo test --release`, `cargo build --release`, `cargo run --release`, `cargo check --release`, `cargo update --aggressive`, `cargo install --force <crate>`, `cargo uninstall <crate>`, `cargo search <query>`, `cargo publish --dry-run`, `cargo login --token <token>`, `cargo logout`, `cargo version`, `cargo help`, `cargo new <project>`, `cargo init`, `cargo add <dependency>`, `cargo remove <dependency>`, `cargo update <dependency>`, `cargo vendor`, `cargo generate-lockfile`, `cargo metadata`, `cargo fix`, `cargo bisect`, `cargo doc --open`, `cargo clippy --fix`, `cargo fmt --check`, `cargo test --release`, `cargo build --release`, `cargo run --release`, `cargo check --release`, `cargo update --aggressive`, `cargo install --force <crate>`, `cargo uninstall <crate>`, `cargo search <query>`, `cargo publish --dry-run`, `cargo login --token <token>`, `cargo logout`, `cargo version`, `cargo help`
- **Forbidden commands**: `node`, `npm`, `yarn`, `pnpm`
- **Read Lessons**: Always check `.github/tasks/lessons.md` for relevant lessons before starting work on a task. Apply the rules from any relevant lessons to avoid repeating mistakes.
- **Document Lessons**: After any correction or feedback from the user, immediately update `.github/tasks/lessons.md` with a description of the mistake pattern and a rule to prevent it in the future. This creates a feedback loop for continuous improvement.
- **Check Todo before Done**: Before marking a task as complete, review the original todo description in `.github/tasks/todo.md` to ensure all requirements are met and the implementation aligns with the plan.
- **Read the tsconfig.json**: Before writing any TypeScript code, read the `tsconfig.json` file to understand the compiler options and project structure. This will help you write code that is compatible with the project's TypeScript configuration and avoid common pitfalls.
- **Read the package.json**: Before adding any dependencies or scripts, read the `package.json` file to understand the existing dependencies, scripts, and project metadata. This will help you maintain consistency and avoid conflicts with existing packages or scripts.
- **Never use Emojis in docs or titles**: Avoid using emojis in any documentation, commit messages, or titles. This maintains a professional tone and ensures clarity for all users, including those who may have difficulty interpreting emojis.
- **Never trust terminal exit codes to verify a service is running**: Terminal context shows stale exit codes and last commands — they do not reflect current process state. Always verify a service is alive by calling its actual endpoint or functionality (e.g. an MCP tool call, a `curl` to a health endpoint, or polling a known route). If the MCP server responds, the API is running. Do not assume a service is down because a terminal shows exit code 1 or 130.
- **Verify service health before debugging**: Before investigating why a feature is not working, confirm the relevant service is actually running by probing it directly. Only proceed to code-level debugging after confirming the service is up and reachable.

---

## Workflow orchestration

### 1. Plan Mode Default
- Enter plan mode for any non-trivial task (3+ steps, arhitectural changes, multiple files affected, etc.)
- If something goes sideways, STOP and re-plan immediatly - don't keep pushing.
- Use plan mode for verification steps, not just building
- Write detailed specs upfront to reduce ambiguity and back-and-forth during implementation.

### 2. Subagent Strategy
- Use subagents liberally to keep main context window, clean.
- Offload research, exploration, and parallel analysis to subagents.
- For complex problems, throw more compute at it via subagents instead of trying to do it all in one agent.
- One task per subagent for focused execution

### 3. Self-improvement loop
- After ANY correction from the user: update `.github/tasks/lessons.md` with the pattern of the mistake.
- Write rules for yourself that prevent the same mistake.
- Ruthlessly iterate on these lessons until the same mistake rate drops.
- Review lessons at session start for relevant project.

### 4. Verification before done
- Never mark task complete without proving it works.
- Diff Behavior between main and your changes when relevant
- Ask yourself: "Would staff engineer approve this?"
- Rrun tests, check logs, demonstration correctness

### 5. Demand elegance (Balanced)
- For non-trivial changes: Paus and ask "is there a simpler or more elegant way?"
- If a fix feel hacky: "Knowing everything I know now, implement the elegant solution instead of the quick fix"
- Skip this for simple, obvious fixes - don't over-enggineer
- Challenge your own work before presenting it

### 6. Automatically bug fixing
- When given a bug report: Just fix it. Don't ask for hand-holding.
- Point at logs, errors, failing tests - then resolve them
- Zero context switching required from the user
- Go fix failing tests without being told how
- Go fix failing linting without being todl to

## Task management

1. **Plan first**: Write plan to `.github/tasks/todo.md` with checkable items.
2. **Verify Plan**: Check in before starting implementation
3. **Track progress*:: Mark items complete as you go
4. **Explain changes**: High level summary at each step
5. **Document results**: Add review section to `.github/tasks/todo.md`
6. **Cappture lessons**: Update `.github/tasks/lessons.md` after corrections

## Format of todos
- Write down a Goal
- A definition of done (what does success look like)
- Break down into steps with checkboxes: `- [ ] Step description`
- Make sure there is a test for each step that can be verified before marking it done.
- Write a brief explanation of the purpose of each step, so it's clear why it's necessary and how it contributes to the overall goal.
- For complex tasks, include a high-level summary of the approach before the step-by-step breakdown.
- After implementation, add a review section at the bottom of the todo with a summary of what was done, any challenges faced, and how they were overcome. This helps create a record of the work and can be useful for future reference.

## Core principles

- **Simpplicity first**: Make very change as simple as possible. Impact minimal code.
- **No laziness**: Find root causes. No temporary fixes. Senior developer standard.
- **Minimat impact**: Changes should only touch waht's necessary. Avoid introducing bugs.

---