# C/C++ Project Profile

This profile is mandatory when the workspace uses C/C++: `CMakeLists.txt`,
`CMakePresets.json`, `compile_commands.json`, `Makefile`, `meson.build`, `configure`,
exists, or `.c`, `.cc`, `.cpp`, `.cxx`, `.h`, `.hh`, `.hpp`, `.hxx` files are affected.

## Stack Detection

- First find the repo-approved workflow in README/CONTRIBUTING, `Makefile`, CI config,
  `CMakePresets.json`, `CMakeLists.txt`, `compile_commands.json`, `meson.build`, and local docs.
- Do not create a new build workflow when the repository already has a documented workflow or existing build dir.
- If the project is mixed-stack, apply this profile only to the C/C++ part and also read
  profiles for other affected stacks.
- If multiple build systems compete, choose the one used by CI/docs or by an existing build artifact
  near the workspace; if the choice is not obvious, record the ambiguity.
- Before choosing a command, check the common `generic.md` profile: repo-approved commands and scoped checks
  take priority over local guesses.

## Executor Rules

- Change code minimally within the affected scope; account for ABI/API, headers, include graph,
  generated files, compile definitions, and platform-specific branches.
- Before commit, format only changed C/C++ files and only when the repo has
  an explicit formatter/config: `.clang-format`, documented command, CI check, or Make target.
- Do not run broad `clang-format -i` across the whole project without a direct user command.
- For compile verification, prefer a repo-approved command:
  `make <target>`, `cmake --build --preset <preset>`, `cmake --build <build-dir>`,
  `ninja -C <build-dir>`, `meson compile -C <build-dir>`, or documented equivalent.
- For tests, prefer a repo-approved test command: `ctest --test-dir <build-dir>`,
  `ctest --preset <preset>`, `ninja -C <build-dir> test`, `make test`, or documented equivalent.
- If you add/change tests, run the new/changed tests and the relevant target/module scope.
- Do not delete or recreate a build directory without direct necessity; this may be expensive
  and can destroy local configuration.
- In outbox, list exact verification commands, build dir/preset/target, skipped checks/failures,
  and the reason for command selection.

## Reviewer Rules

- The reviewer must not run mutating formatters/fixers/auto-fixers:
  `clang-format -i`, `clang-tidy -fix`, `cmake --build` targets with auto-fix side effects, or equivalents.
- For format verification, use non-mutating checks if the repo supports them:
  `clang-format --dry-run --Werror <changed-files>` or a documented check target.
- Run `clang-tidy` only when `compile_commands.json` or a documented command exists; scope must
  be limited to changed files/targets. Do not run broad linting scans unnecessarily.
- Build/test commands are evidence, but they do not replace reading the diff, headers, call sites,
  ownership/lifetime/threading assumptions, error handling, and integration contracts.

## CMake Guidance

- If `CMakePresets.json` exists, prefer presets over manually creating a build dir.
- If an existing build dir has `CMakeCache.txt`, use it for scoped build/test
  unless this conflicts with repo docs.
- If no build dir exists and repo docs do not explain how to configure the project, **do not run
  a guessed `cmake -S . -B build` or invent configure flags**. Record a skipped build
  with the reason and state what command is needed.
- Do not edit generated files manually if the build system creates them; edit the source of truth.

## Build System Decision Tree

### CMake presets

- If `CMakePresets.json` exists, read it first and choose a documented preset.
- For executor build, prefer `cmake --build --preset <preset>`.
- For executor tests, prefer `ctest --preset <preset>` or a documented test preset.
- The reviewer should not run mutating configure/build steps only for review when it can inspect
  diff/config or use non-mutating documented checks.

### Existing build dir + compile_commands.json

- If an existing build dir has `CMakeCache.txt` and/or `compile_commands.json`, use it as
  the source of truth for scoped build/test/lint unless repo docs say otherwise.
- For compile checks, prefer a build target related to changed files, not a full rebuild.
- `clang-tidy` is allowed only with a compile database and scoped files/targets.
- Do not regenerate the compile database without a repo-approved configure command.

### Ninja

- If an existing build dir uses Ninja (`build.ninja`), prefer `ninja -C <build-dir> <target>`.
- For tests, use `ninja -C <build-dir> test` only if that target exists or is documented.
- Do not run plain `ninja` from the workspace root if the build dir is not obvious.

### Make

- If a top-level `Makefile` exists, read targets first (`make help`, docs, CI, or the Makefile itself).
- Prefer documented targets: `make test`, `make check`, `make lint`, `make <component>`.
- Do not run arbitrary `make` without a target if the default target may be expensive or mutating.

### Meson

- If `meson.build` exists, look for an existing build dir and repo docs.
- Executor build: `meson compile -C <build-dir>` or documented target.
- Tests: `meson test -C <build-dir>` or documented subset.
- If no build dir exists and setup options are unknown, do not run a guessed `meson setup build`.

### Bazel

- If `WORKSPACE`, `MODULE.bazel`, or `BUILD.bazel` exists, prefer documented Bazel commands from
  README/CI.
- If docs are absent, scoped query/build/test is allowed only when the target is obvious from changed
  BUILD files.
- Do not run broad `bazel test //...` without assessing cost and necessity.

### Autotools/configure

- If `configure`, `configure.ac`, or `Makefile.am` exists, use the documented workflow.
- Do not run `./configure` with guessed flags unless local docs/CI show the required options.
- If a configured build dir already exists, use documented `make`/`make check` scope.

## Risk Checklist

- Check ABI/API compatibility for public headers and exported symbols.
- Check include dependencies, forward declarations, and possible include cycles.
- Check ownership/lifetime, raw pointers/references, move/copy semantics, and exception safety.
- Check threading/concurrency assumptions, locks, atomics, and callback lifetimes.
- Check platform-specific code paths, feature flags, compiler versions, and warning-as-error policy.
- Check serialization/wire/db contracts if C/C++ code participates in integrations.

## Command Ambiguity

- C/C++ commands are not as universal as Rust/Cargo: there is no single safe equivalent of
  `cargo test` for all projects.
- **If there is no `compile_commands.json`, `CMakePresets.json`, existing build dir, docs, or CI command,
  do not replace that gap with a guessed local command. Do not run guessed configure/build commands.**
- Explicitly state the skipped check, why it was skipped, what files/configs were inspected, and what command is needed
  from a human or CI.
