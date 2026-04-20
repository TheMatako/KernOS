# KernOS — GitHub Workflow & Contributing Guide

This Github workflow has been developped with the help of Claude and is subject to change as I'm quite new in 
Github professional working method and may change a few things but here is the main idea. As always, feel free
to contact me if you have any suggestion regarding the Github process described below.

> This document defines the development workflow, branching strategy, commit conventions,
> and code review process used in the KernOS project.
> It exists to ensure consistency, traceability, and professional standards throughout development.

---

## Table of Contents

1. [Branch Strategy](#branch-strategy)
2. [Branch Naming Convention](#branch-naming-convention)
3. [Commit Convention](#commit-convention)
4. [Pull Request Process](#pull-request-process)
5. [CI Pipeline](#ci-pipeline)
6. [Day-to-Day Workflow](#day-to-day-workflow)
7. [Kernel Brick Workflow](#kernel-brick-workflow)

---

## Branch Strategy

KernOS uses a **trunk-based development** model with short-lived feature branches.

```
main (protected)
 │
 ├── feat/brick-1-bootstrap      ← active development
 ├── fix/serial-baud-rate        ← bug fix
 ├── docs/update-boot-process    ← documentation update
 └── refactor/idt-cleanup        ← code cleanup
```

### Rules

| Branch | Purpose | Direct push allowed |
|--------|---------|-------------------|
| `main` | Stable, always compiles | ❌ Never |
| `feat/*` | New feature or kernel brick | ✅ Yes |
| `fix/*` | Bug fix | ✅ Yes |
| `docs/*` | Documentation only | ✅ Yes |
| `refactor/*` | Code cleanup, no new feature | ✅ Yes |
| `chore/*` | Tooling, CI, Makefile | ✅ Yes |

### The Golden Rule

> `main` must **always** compile and **always** represent a working state of the OS.
> Nothing broken ever lands on `main`.

---

## Branch Naming Convention

```
<type>/<short-description>
```

The description must be lowercase, hyphen-separated, and as concise as possible.

### Examples

```bash
feat/brick-1-bootstrap
feat/brick-2-gdt
feat/brick-2-idt
feat/brick-3-pmm
feat/brick-4-vmm-paging
feat/brick-5-scheduler
feat/brick-6-syscall
feat/brick-7-serial-driver
feat/brick-7-keyboard-driver
feat/brick-8-vfs
feat/brick-9-network-stack
feat/brick-10-shell
fix/pmm-double-free
fix/idt-page-fault-handler
fix/bootloader-elf-load
docs/readme-update
docs/boot-process
refactor/serial-driver-cleanup
chore/makefile-lint-target
chore/ci-add-clippy
```

---

## Commit Convention

KernOS follows the **Conventional Commits** specification.
Every commit message must follow this format :

```
<type>(<scope>): <short description>

[optional body]

[optional footer]
```

### Types

| Type | When to use |
|------|------------|
| `feat` | Adding new functionality |
| `fix` | Fixing a bug |
| `docs` | Documentation changes only |
| `refactor` | Code restructuring, no behavior change |
| `chore` | Tooling, CI, build system |
| `test` | Adding or updating tests |
| `perf` | Performance improvement |
| `style` | Formatting, no logic change |

### Scopes

| Scope | What it covers |
|-------|---------------|
| `bootloader` | Everything in `bootloader/` |
| `kernel` | General kernel code |
| `gdt` | Global Descriptor Table |
| `idt` | Interrupt Descriptor Table |
| `pmm` | Physical Memory Manager |
| `vmm` | Virtual Memory Manager |
| `scheduler` | Process scheduler |
| `syscall` | System call interface |
| `serial` | Serial port driver |
| `keyboard` | Keyboard driver |
| `disk` | Disk driver |
| `vfs` | Virtual File System |
| `net` | Network stack |
| `shell` | Shell and userspace |
| `ci` | GitHub Actions pipeline |
| `makefile` | Build system |

### Examples

```bash
# Good commit messages
feat(serial): initialize UART at 0x3F8 with 115200 baud
feat(pmm): implement frame bitmap allocator
fix(idt): correct page fault handler stack alignment
docs(boot-process): document ELF loading sequence
refactor(gdt): simplify descriptor entry construction
chore(ci): add clippy job to GitHub Actions
perf(pmm): use bitwise scan for faster free frame lookup

# Bad commit messages — never do this
fix stuff
update
wip
asdfgh
```

### Commit Frequency

Commit **often** — every time a small, self-contained unit of work is done.
A commit should answer the question : *"What does this change do ?"*

```bash
# Too few commits — hard to understand history
feat(brick-2): implement GDT and IDT and interrupts and APIC

# Good granularity — clear history
feat(gdt): define kernel code and data segment descriptors
feat(gdt): write lgdt wrapper and load GDT at boot
feat(idt): define IDT entry structure and table
feat(idt): implement exception handlers 0-31
feat(idt): install and load IDT via lidt
```

---

## Pull Request Process

### When to open a PR

Open a PR when a **complete, testable unit of work** is ready :
- A full kernel brick is implemented and tested in QEMU
- A bug is fixed and verified
- Documentation is updated

Do **not** open a PR for work in progress. Use `git stash` or keep committing
on your branch until the work is ready.

### PR Title Format

Follow the same convention as commits :

```
feat(brick-3): implement Physical Memory Manager (PMM)
fix(idt): handle double fault with separate stack
docs: add VMM design notes to boot-process.md
```

### PR Description Template

Every PR must include the following sections :

```markdown
## Description
<!-- What does this PR do ? Why is it needed ? -->

## Changes
<!-- Bullet list of what was added, modified, or removed -->
- Added `pmm::init()` which parses the boot info memory map
- Implemented `alloc_frame()` using a bitmap of 4KB frames
- Implemented `free_frame()` with double-free detection

## Testing
<!-- How was this tested ? What was observed in QEMU ? -->
- Tested in QEMU with `make run`
- Serial output confirms correct frame count at boot
- Allocating and freeing 100 frames produces no errors

## Dependencies
<!-- Does this PR depend on another PR or branch ? -->
- Depends on #12 (Boot Info Structure)

## Checklist
- [ ] Compiles without warnings (`make build`)
- [ ] `make run` works in QEMU
- [ ] Code is commented and documented
- [ ] Commit messages follow the convention
- [ ] No debug code left behind
```

### Review Process

Since KernOS is currently a solo project, self-review is the standard.
Before merging, always :

1. Re-read the entire diff on GitHub
2. Verify all CI checks are green
3. Run `make run` one final time locally
4. Check that no TODO or debug print was accidentally left in

### Merging

Always use **Squash and Merge** when the branch has many small WIP commits,
or **Merge Commit** when every commit is clean and meaningful.

After merging :

```bash
# Switch back to main and pull the merged changes
git checkout main
git pull

# Delete the local branch — it is no longer needed
git branch -d feat/brick-1-bootstrap
```

---

## CI Pipeline

Every push and every PR triggers the CI pipeline automatically.
A PR **cannot be merged** if any CI job fails.

### Jobs

| Job | What it does | Command |
|-----|-------------|---------|
| `build` | Compiles bootloader and kernel | `cargo +nightly build` |
| `fmt` | Checks code formatting | `cargo +nightly fmt -- --check` |
| `clippy` | Runs the Rust linter | `cargo +nightly clippy -- -D warnings` |

### Running CI checks locally before pushing

Always run these before opening a PR :

```bash
# Format the code automatically
make fmt

# Run the linter
make lint

# Build everything
make build

# Test in QEMU
make run
```

If all four pass locally, the CI will pass on GitHub.

---

## Day-to-Day Workflow

This is the exact sequence of commands to use for every piece of work.

### Starting a new task

```bash
# 1. Make sure main is up to date
git checkout main
git pull

# 2. Create a new branch from main
git checkout -b feat/brick-1-bootstrap
```

### During development

```bash
# See what has changed
git status

# See the exact changes line by line
git diff

# Stage specific files
git add kernel/src/drivers/serial.rs

# Stage everything
git add .

# Commit with a clear message
git commit -m "feat(serial): initialize UART at 0x3F8 with 115200 baud"
```

### Pushing and opening a PR

```bash
# Push the branch to GitHub
git push -u origin feat/brick-1-bootstrap

# GitHub will show a banner to open a PR — click it
# Fill in the PR description using the template above
```

### After the PR is merged

```bash
# Go back to main and pull
git checkout main
git pull

# Delete the now-merged local branch
git branch -d feat/brick-1-bootstrap

# Start the next task
git checkout -b feat/brick-2-gdt
```

---

## Kernel Brick Workflow

Each kernel brick follows this specific process :

```
1. Create branch          git checkout -b feat/brick-N-name
        │
2. Read the theory        Understand the brick before writing a line
        │
3. Implement              Write the code with full comments
        │
4. Test in QEMU           make run — verify serial output shows init OK
        │
5. Lint and format        make fmt && make lint
        │
6. Commit                 One commit per logical sub-unit
        │
7. Open PR                Fill in the PR description template
        │
8. CI passes              All three jobs green
        │
9. Merge                  Squash and merge into main
        │
10. Update roadmap        Check the brick off in README.md
```

### Branch lifetime

A brick branch lives for the duration of implementing that brick only.
Once merged, it is deleted. The next brick gets a fresh branch from `main`.

### Commit structure for a brick

```bash
# Example for Brick 2 — GDT + IDT

feat(gdt): define GDT entry structure and flags
feat(gdt): implement GDT table with kernel code/data segments
feat(gdt): load GDT via lgdt and switch segments
feat(idt): define IDT gate descriptor structure
feat(idt): implement CPU exception handlers 0-31
feat(idt): implement IRQ handlers and APIC timer
feat(idt): load IDT via lidt
docs(gdt): add inline comments explaining each descriptor flag
```

---

*This document is a living reference — update it whenever the workflow evolves.*