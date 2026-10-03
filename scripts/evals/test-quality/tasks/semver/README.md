# semver

Compare version strings by Semantic Versioning 2.0.0 precedence. Implement it in
`semver/__init__.py`.

## `compare(a, b) -> int`

Returns `-1` if `a` has lower precedence than `b`, `0` if equal, `1` if higher. Exactly these
three values. Raises `ValueError` if either argument is not a valid version.

## Valid versions

`MAJOR.MINOR.PATCH`, optionally followed by `-PRERELEASE`, optionally followed by `+BUILD`.

- `MAJOR`, `MINOR`, `PATCH`: non-negative decimal integers without leading zeros (`0` is fine,
  `01` is not). Exactly three of them.
- `PRERELEASE` and `BUILD`: one or more dot-separated identifiers, each a non-empty run of ASCII
  letters, digits and `-`. A pre-release identifier made only of digits is numeric and must not
  have leading zeros (`01` is invalid in a pre-release; it is fine in build metadata).
- Anything else is invalid: `""`, `"1.2"`, `"1.2.3.4"`, `"1.0.0-"`, `"1.0.0-a..b"`, `"1.0.0+"`,
  `"v1.0.0"`.

## Precedence

1. Compare `MAJOR`, `MINOR`, `PATCH` numerically, in that order.
2. If those are equal, a version without a pre-release ranks above one with a pre-release:
   `1.0.0-alpha < 1.0.0`.
3. Two pre-releases compare identifier by identifier, left to right:
   - two numeric identifiers compare numerically;
   - two alphanumeric identifiers compare lexically in ASCII order;
   - a numeric identifier ranks below an alphanumeric one;
   - if all shared identifiers are equal, the longer list ranks higher.
4. Build metadata is ignored: `1.0.0+a` and `1.0.0+b` are equal.

## Example

`1.0.0-alpha < 1.0.0-alpha.1 < 1.0.0-alpha.beta < 1.0.0-beta < 1.0.0-beta.2 < 1.0.0-beta.11
< 1.0.0-rc.1 < 1.0.0`
