# Core Lightning plugin for nostr wallet connect

> **Warning**
> This is an ALPHA implementation [NIP47](https://github.com/nostr-protocol/nips/blob/master/47.md), that has **both** privacy implications and risk of funds. You should not run this unless you understand those risks and ideally reviewed the code. 

You can add the plugin by copying it to CLN's plugin directory or by adding the following line to your config file:

```
plugin=/path/to/nostr-wallet-connect
```
## CLN Config

Since zap invoices include only description hash and the description is not currently included in nip47 `allow-deprecated-apis=true` must be set. 

## Options
`cln-nostr-wallet-connect` exposes the following config options that can be included in CLN's config file or as command line flags:
* `nostr_connect_wallet_nsec`: Nostr Key to publish events, optional: will auto generate if not defined
* `nostr_connect_client_secret`: This should not be your main secret key, you should create a new keypair as securely as possible as it can authorize spending funds. optional 
* `nostr_connect_relay`: Nostr relay to connect to. You should use a private relay with auth to reduce leaking metadata
* `nostr_connect_max_invoice`: Max amount in msats of an invoice to pay. Defaults to 5000000
* `nostr_connect_hour_limit`: Max msat to spend per hour. Defaults to 10,000,000.
* `nostr_connect_day_limit`: Max msat to spend per day. Defaults to 35000000
* `nostr_connect_config_path`: Path to configuration file. Defaults to data directory

## License

Code is under the [BSD 3-Clause License](LICENSE-BSD-3)

## Contribution

All contributions welcome.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, shall be licensed as above, without any additional terms or conditions.

## Contact

I can be contacted for comments or questions on nostr at _@thesimplekid.com (npub1qjgcmlpkeyl8mdkvp4s0xls4ytcux6my606tgfx9xttut907h0zs76lgjw) or via email tsk@thesimplekid.com.


## Credit
https://github.com/elsirion/clnurl

https://github.com/jb55/cln-nostr-zapper

https://github.com/0xtrr/nostr-tool
