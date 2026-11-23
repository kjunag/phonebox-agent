# PhoneBox agent

Agent PhoneBox dla Linuksa, napisany w Ruście. Telefon łączy się z komputerem i pokazuje pulpit: obciążenie, procesy, media, baterię, skróty i resztę widżetów panelu.

To samodzielny program. Nie uruchamia oryginalnego agenta .NET.

## Uruchomienie

Potrzebny jest Rust ze stabilnym toolchainem (edycja 2024) oraz `pkg-config` i OpenSSL.

```sh
cargo run --locked
```

Ustawienia są na `http://127.0.0.1:8765`. Port zmienia zmienna `PORT`.

Gdy w konfiguracji włączone jest Wi-Fi, agent nasłuchuje też na `https://0.0.0.0:8766`. Certyfikat i odcisk palca biorą się z `lan-cert.pfx` obok pliku konfiguracji. Telefon musi podać token w nagłówku `Authorization: Bearer …`.

Konfiguracja to `~/.config/PhoneBox/config.json` (JSON z komentarzami). Inną ścieżkę podaje się przez `--config`.

Pliki panelu leżą w `assets/`. Przy uruchomieniu z innego katalogu wskaż je zmienną `PHONEBOX_ASSETS`.

## Co widać na telefonie

- stan komputera: procesor, pamięć, dyski, sieć, karta graficzna
- procesy, z ikoną i możliwością zakończenia
- odtwarzacz i okładka utworu (`playerctl`)
- głośność (`wpctl`)
- bateria laptopa i urządzeń zgłaszanych przez UPower
- kalendarz, GitHub, sesje asystentów, skróty, parowanie i zgody Claude

Nie każdy widżet z oryginalnego PhoneBox jest już przeniesiony.

## Programy pomocnicze

Agent działa bez nich, ale część widżetów zostaje pusta:

| Program | Do czego |
| --- | --- |
| `playerctl` | odtwarzacz i okładka |
| `wpctl` | głośność |
| `upower` | baterie |
| `curl` | okładki z sieci i haczyk Claude |
| `gh` | GitHub |

## Sprawdzenie

```sh
cargo test --locked
```

GitHub Actions uruchamia testy i Clippy. Nie buduje osobnej binarki release.
