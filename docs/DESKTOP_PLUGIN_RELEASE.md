# Aizen Desktop plugin release v1

The v1 release contract is an optional Desktop presentation delivered through the
authenticated Aizen Marketplace. Core and Desktop compile the same Rust validator.
The Marketplace preserves the submitted UTF-8 bytes and verifies the same constraints
with a Go validator. All three use the same reference manifest. The
[JSON Schema](../examples/desktop-plugins/aizen-release.schema.json) supports editor
validation. The validators additionally check original bytes, ID uniqueness and
duplicate JSON keys.

Validate the example from a source checkout:

```sh
cargo run --bin aizen -- plugin validate-release examples/desktop-plugins/aizen-release.json
```

Release file limit is 32 KiB. Root keys are `schemaVersion`, `slug`, `version` and
`desktop`. The Desktop object contains `uiApiVersion` and `views`. Both protocol
versions are integer 1. Slugs and per-parent unique IDs are 2–64 ASCII characters
matching `^[a-z0-9][a-z0-9-]{1,63}$`. Versions are three zero-or-positive decimal
integers without leading zeros, separated by dots, up to 32 bytes. Titles are
nonblank, up to 160 UTF-8 bytes, with control characters and bidi control marks excluded.

A release has 1–6 views. Each view declares `type`, `id` and `title`. A `calculator`
view uses those fields. A `board` view additionally declares 2–8 `columns` with
`id` and `title`. Unknown and duplicate fields are rejected.

Desktop validates the entitlement response, version, slug and SHA-256 before
rendering. The renderer owns the supported operations. Drafts keep temporary state
in memory. Board data is scoped to the installed account and Marketplace product.
Keep view and column IDs stable across label edits. Removed columns fall back to
the first column. Every live opening checks the entitlement online.

Publish with delivery type `desktop_release` and submit the manifest as the
`desktop_release` string alongside the matching product slug and version. Follow
the [seller publishing guide](../examples/desktop-plugins/SELLER.vi.md) for review
and listing information.

Before releasing the three applications together, check the shared files across
the checkouts:

```sh
python scripts/check_desktop_plugin_contract.py --desktop ../aizen-desktop-plugin --pilot ../aizen-admin/pilot --web ../aizen_web
```

Native Memory packages retain their own runtime manifest described in
[Memory plugins](MEMORY_PLUGINS.md). This release format covers the built-in Desktop
views. Additional view runtimes or access to host functions require explicit
protocol support and compatibility tests.
