# antbot

Bot de mean-reversion du premium HIP-3 `xyz:ANT` sur Hyperliquid (Rust / tokio).
Mode par défaut : **paper** (données de marché mainnet en direct, fills simulés, aucune clé, aucun appel à `/exchange`).

> Documentation complète à venir. Ce n'est pas un conseil en investissement.

```sh
cargo run -- check-config            # valide config/default.toml et le garde-fou de mode
cargo run -- run                     # paper (boucle en cours d'implémentation)
```
