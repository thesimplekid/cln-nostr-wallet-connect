# Changelog

[Unreleased]

## Change
- Update cln-rpc
- Update cln-plugin
- Update nostr-sdk

## [0.2.3]
### Fixed
- Fix: Shutdown plugin with lightningd

### Change
- deps: bump `nostr` to `0.20.0`

## [0.2.2]
### Change
- Improvement: Correct use for optional configs
- Improvement: Check zap request amount matches invoice amount

## [0.2.0]
### Change
- Fix: Increment pay index count for non zaps [(@denis2342)](https://github.com/denis2342) 
- Refactor: use filter map on relays to avoid second loop
### Add
- Improvement: Add option to config for path of last pay index tip

## [0.1.2] 
### Add 
- Improvement: Add preimage tag to zap note

## [0.1.1]
### Add
- Improvement: Save paid invoice index tip to file. Restart from saved tip
### Fixed
- Fix: Use invoice description for zap request description tag [(@denis2342)](https://github.com/denis2342)
