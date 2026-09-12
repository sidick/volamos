# assets/

Local-only holding area for files this project needs at times (real
Kickstart ROM images, other Hyperion/Commodore-copyrighted binaries,
etc.) but can never redistribute -- same posture already used for the
NDK (see `.gitignore`'s note on `tools/ndk_verify.py`) and for the
amitools/AmiBake checkouts referenced from `docs/plan.md`.

Nothing under here is tracked by git except this README (see
`.gitignore`). Never vendor a Kickstart ROM, NDK archive, or other
non-redistributable asset anywhere else in the repo tree.

Suggested layout (not enforced):

```
assets/
  kickstart/
    kick40.72.rom       # KS/WB 3.1, this project's Phase 1 target
    kick34.5.rom        # KS/WB 1.3
    ...
```

Tools that need a path under here (e.g. a local three-way comparison
harness against real Kickstart hardware via Copperline) take it as an
explicit CLI argument -- never a default baked into the repo -- so a
clean checkout with an empty `assets/` still works for everything that
doesn't need these files.
