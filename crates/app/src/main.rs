//! Command-line front end for ym-sync.
//!
//! Every machine runs `play` and joins a relay room; the relay holds the queue
//! and the playhead, so any of them may skip, pause or add tracks and the rest
//! follow. Each machine uses its own Yandex account, so simultaneous playback is
//! never blocked; tracks are identified by their numeric id, which is the same on
//! every account.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};
use tokio::sync::mpsc;
use ymsync::api::{self, Track, YandexMusic};
use ymsync::config::Config;
use ymsync::engine::{self, Command as EngineCommand, Snapshot};
use ymsync::fmt_ms;
use ymsync::player::Player;
use ymsync_proto::TrackRef;

/// How often the status line is refreshed when nothing notable changes.
const STATUS_EVERY: Duration = Duration::from_secs(2);

#[derive(Parser, Debug)]
#[command(
    name = "ymsync",
    version,
    about = "Синхронное воспроизведение Яндекс Музыки на нескольких ПК"
)]
struct Cli {
    /// Путь к config.toml (по умолчанию — каталог настроек пользователя)
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Переопределить адрес релея
    #[arg(long, global = true, value_name = "URL")]
    relay: Option<String>,

    /// Переопределить имя комнаты
    #[arg(long, global = true, value_name = "NAME")]
    room: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Показать путь к конфигу и создать шаблон, если его нет
    Config,

    /// Проверить весь путь до звука: токен, поиск, подпись ссылки, загрузка, декодирование
    Probe {
        #[arg(default_value = "Кино Группа крови")]
        query: String,
    },

    /// Найти треки и показать их id
    Search {
        #[arg(required = true)]
        query: Vec<String>,

        #[arg(long, default_value_t = 10)]
        limit: usize,
    },

    /// Подключиться к комнате и играть вместе с остальными
    ///
    /// Источник необязателен: без него подключаемся к тому, что уже играет.
    /// Псевдонимы master и slave остались от протокола 2, когда роли ещё были
    /// разными.
    #[command(alias = "master", alias = "slave")]
    Play {
        #[command(flatten)]
        source: SourceArgs,
    },
}

/// What to fill the queue with. At most one; none means "join whatever is
/// already playing".
#[derive(Args, Debug)]
#[group(required = false, multiple = false)]
struct SourceArgs {
    /// id трека или ссылка music.yandex.ru/album/<a>/track/<id>
    #[arg(long, value_name = "ID")]
    track: Option<String>,

    /// Первый результат поиска
    #[arg(long, value_name = "QUERY")]
    search: Option<String>,

    /// Альбом целиком: id или ссылка
    #[arg(long, value_name = "ID")]
    album: Option<String>,

    /// Плейлист целиком: логин/номер или ссылка
    #[arg(long, value_name = "OWNER/KIND")]
    playlist: Option<String>,

    /// Плейлист «Мне нравится» этого аккаунта
    #[arg(long)]
    likes: bool,

    /// «Моя волна»: бесконечная станция, очередь пополняется сама
    #[arg(long)]
    wave: bool,
}

enum Source {
    Track(String),
    Search(String),
    Album(String),
    Playlist(String, String),
    Likes,
    /// Not a list at all: the engine follows the station and asks it for more.
    Wave,
}

impl SourceArgs {
    /// `None` means no source was asked for: join the room as it is.
    fn parse(&self) -> Result<Option<Source>> {
        if let Some(raw) = &self.track {
            return Ok(Some(Source::Track(api::parse_track_id(raw)?)));
        }
        if let Some(query) = &self.search {
            return Ok(Some(Source::Search(query.clone())));
        }
        if let Some(raw) = &self.album {
            return Ok(Some(Source::Album(api::parse_album_id(raw)?)));
        }
        if let Some(raw) = &self.playlist {
            let (owner, kind) = api::parse_playlist_ref(raw)?;
            return Ok(Some(Source::Playlist(owner, kind)));
        }
        if self.likes {
            return Ok(Some(Source::Likes));
        }
        if self.wave {
            return Ok(Some(Source::Wave));
        }
        Ok(None)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("ymsync=info")),
        )
        .with_target(false)
        .without_time()
        .init();

    let cli = Cli::parse();
    let (mut cfg, config_path) = Config::load(cli.config.as_deref())?;
    if let Some(relay) = cli.relay {
        cfg.relay = relay;
    }
    if let Some(room) = cli.room {
        cfg.room = room;
    }

    match cli.command {
        Command::Config => show_config(&cfg, &config_path),
        Command::Probe { query } => probe(&cfg, &query).await,
        Command::Search { query, limit } => search(&cfg, &query.join(" "), limit).await,
        Command::Play { source } => {
            // Validate arguments before opening the audio device or the network.
            let source = source.parse()?;
            run(&cfg, source).await
        }
    }
}

async fn run(cfg: &Config, source: Option<Source>) -> Result<()> {
    let api = Arc::new(YandexMusic::new(cfg.require_yandex_token()?)?);

    // Resolve the queue before touching audio or the relay, so a bad query fails
    // immediately instead of half-way into a session.
    let queue = match &source {
        Some(source) => resolve_queue(&api, source).await?,
        None => Vec::new(),
    };

    let player = Arc::new(Player::new(cfg.volume)?);
    let handle = engine::spawn(cfg, Arc::clone(&api), player).await?;

    let start = handle.snapshot();
    println!(
        "⇄ комната «{}» на {} (участников: {}, смещение часов {:+} мс, rtt {} мс)",
        cfg.room,
        cfg.relay,
        start.peers,
        start.offset_ms.unwrap_or_default(),
        start.rtt_ms.unwrap_or_default(),
    );

    if queue.is_empty() {
        if matches!(source, Some(Source::Wave)) {
            println!("волна: треки приходят по мере воспроизведения");
            handle.send(EngineCommand::PlayStation {
                id: api::WAVE_STATION.to_string(),
                replace: true,
            });
        } else {
            println!("… играем то, что в комнате; командовать может любой участник");
        }
    } else {
        println!("очередь: {} трек(ов)", queue.len());
        for (position, track) in queue.iter().enumerate().take(10) {
            println!("  {:2}. {track}", position + 1);
        }
        if queue.len() > 10 {
            println!("  … ещё {}", queue.len() - 10);
        }
        handle.send(EngineCommand::SetQueue {
            tracks: queue,
            start: 0,
        });
    }
    print_controls();

    let mut stdin = spawn_stdin_lines();
    let mut stdin_open = true;
    let mut snapshots = handle.subscribe();
    let mut last_status = Instant::now() - STATUS_EVERY;
    let mut last_signature = String::new();

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("выход");
                break;
            }

            line = stdin.recv(), if stdin_open => {
                match line {
                    // No console behind stdin (piped, or launched detached):
                    // stop taking commands but keep playing.
                    None => stdin_open = false,
                    Some(line) => match parse_input(&line) {
                        Action::Quit => break,
                        Action::Help => print_controls(),
                        Action::Status => {
                            print_status(&handle.snapshot());
                            last_status = Instant::now();
                        }
                        Action::Engine(command) => handle.send(command),
                        Action::Unknown(text) => {
                            println!("не понял: {text} (введите ? для подсказки)");
                        }
                    },
                }
            }

            changed = snapshots.changed() => {
                if changed.is_err() {
                    break;
                }
                let snapshot = snapshots.borrow_and_update().clone();
                // Print on anything notable, and otherwise just tick along.
                let signature = format!(
                    "{:?}|{}|{}|{:?}",
                    snapshot.track.as_ref().map(|t| &t.track_id),
                    snapshot.playing,
                    snapshot.loading,
                    snapshot.notice,
                );
                if signature != last_signature || last_status.elapsed() >= STATUS_EVERY {
                    print_status(&snapshot);
                    last_signature = signature;
                    last_status = Instant::now();
                }
            }
        }
    }

    handle.send(EngineCommand::Shutdown);
    handle.join().await
}

async fn resolve_queue(api: &YandexMusic, source: &Source) -> Result<Vec<TrackRef>> {
    let tracks: Vec<Track> = match source {
        Source::Track(id) => vec![api.track(id).await?],
        Source::Search(query) => {
            let found = api.search_tracks(query, 1).await?;
            if found.is_empty() {
                bail!("по запросу «{query}» ничего не найдено");
            }
            found
        }
        Source::Album(id) => api.album_tracks(id).await?,
        Source::Playlist(owner, kind) => api.playlist_tracks(owner, kind).await?,
        Source::Likes => api.liked_tracks().await?,
        // The station is asked for tracks by the engine, as the queue runs down.
        Source::Wave => Vec::new(),
    };
    Ok(tracks.iter().map(Track::to_track_ref).collect())
}

fn print_status(snapshot: &Snapshot) {
    let position = if snapshot.loading {
        "загрузка…".to_string()
    } else {
        format!(
            "{} / {}",
            fmt_ms(snapshot.position_ms),
            fmt_ms(snapshot.duration_ms)
        )
    };
    let title = snapshot
        .track
        .as_ref()
        .map_or_else(|| "—".to_string(), TrackRef::to_string);
    let place = if snapshot.queue.is_empty() {
        String::new()
    } else {
        format!(" [{}/{}]", snapshot.index + 1, snapshot.queue.len())
    };

    print!(
        "{} {title}{place}  {position}  участников {}",
        if snapshot.playing { "▶" } else { "⏸" },
        snapshot.peers
    );
    if let Some(drift_ms) = snapshot.drift_ms {
        print!("  рассинхрон {drift_ms:+} мс, rtt {} мс", snapshot.rtt_ms.unwrap_or_default());
    }
    if let Some(notice) = &snapshot.notice {
        print!("  · {notice}");
    }
    println!();
}

/// What a typed line means.
#[derive(Debug, Clone, PartialEq)]
enum Action {
    Quit,
    Help,
    Status,
    Engine(EngineCommand),
    Unknown(String),
}

fn parse_input(line: &str) -> Action {
    // Notepad writes UTF-8 with a byte-order mark and PowerShell prefixes piped
    // stdin with one, so without stripping it the first command from a file or a
    // pipe would never match.
    let text = line.trim_start_matches('\u{feff}').trim();

    match text {
        "q" | "quit" | "exit" => return Action::Quit,
        "?" | "h" | "help" => return Action::Help,
        "" | "s" | "status" => return Action::Status,
        "p" | "pause" => return Action::Engine(EngineCommand::TogglePause),
        "n" | "next" => return Action::Engine(EngineCommand::Next),
        "b" | "prev" => return Action::Engine(EngineCommand::Prev),
        _ => {}
    }

    if let Some(rest) = text.strip_prefix('v') {
        return match rest.trim().parse::<f32>() {
            Ok(percent) => {
                Action::Engine(EngineCommand::SetVolume((percent / 100.0).clamp(0.0, 2.0)))
            }
            Err(_) => Action::Unknown(text.to_string()),
        };
    }
    // `#3` plays the third queue entry; humans count from one.
    if let Some(rest) = text.strip_prefix('#') {
        return match rest.trim().parse::<usize>() {
            Ok(number) if number >= 1 => {
                Action::Engine(EngineCommand::PlayIndex(number - 1))
            }
            _ => Action::Unknown(text.to_string()),
        };
    }
    for (prefix, sign) in [('+', 1), ('-', -1)] {
        if let Some(rest) = text.strip_prefix(prefix) {
            return match parse_seconds(rest) {
                Some(ms) => Action::Engine(EngineCommand::SeekBy(sign * ms)),
                None => Action::Unknown(text.to_string()),
            };
        }
    }
    match parse_seconds(text) {
        Some(ms) => Action::Engine(EngineCommand::SeekTo(ms as u64)),
        None => Action::Unknown(text.to_string()),
    }
}

fn parse_seconds(raw: &str) -> Option<i64> {
    raw.trim()
        .parse::<f64>()
        .ok()
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .map(|seconds| (seconds * 1000.0) as i64)
}

fn print_controls() {
    println!(
        "команды: p — пауза/продолжить · n / b — следующий / предыдущий · #3 — трек из очереди · \
         <сек> — перейти · +30 / -10 — смещение · v 70 — громкость · q — выход"
    );
}

/// Reads stdin in its own task: `Lines::next_line` is not cancel-safe, so
/// polling it directly inside `select!` could swallow typed input.
fn spawn_stdin_lines() -> mpsc::UnboundedReceiver<String> {
    use tokio::io::AsyncBufReadExt;

    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

fn show_config(cfg: &Config, path: &std::path::Path) -> Result<()> {
    println!("конфиг: {}", path.display());
    if path.exists() {
        println!("        файл есть");
    } else {
        Config::write_template(path)?;
        println!("        создан шаблон — впишите yandex_token и room_token");
    }
    println!();
    println!("relay          = {}", cfg.relay);
    println!("room           = {}", cfg.room);
    println!("volume         = {:.0}%", cfg.volume * 100.0);
    println!(
        "токен Яндекса  : {}",
        present(!cfg.yandex_token.trim().is_empty())
    );
    println!(
        "токен комнаты  : {}",
        present(!cfg.room_token.trim().is_empty())
    );
    println!();
    println!("порог коррекции: {} мс", cfg.sync.seek_threshold_ms);
    println!("сверка позиции : каждые {} мс", cfg.sync.correction_interval_ms);
    Ok(())
}

/// Walks the whole unofficial-API path so a breakage surfaces as one clear
/// diagnostic instead of a mysterious playback failure.
async fn probe(cfg: &Config, query: &str) -> Result<()> {
    let api = YandexMusic::new(cfg.require_yandex_token()?)?;

    println!("1/5 аккаунт…");
    let status = api.account_status().await?;
    let name = if status.account.display_name.is_empty() {
        status.account.login.clone()
    } else {
        status.account.display_name.clone()
    };
    let has_plus = status.plus.as_ref().is_some_and(|p| p.has_plus);
    println!(
        "    {name} (uid {}, регион {}), Плюс: {}",
        opt(status.account.uid),
        opt(status.account.region),
        present(has_plus)
    );
    if !has_plus {
        println!("    ⚠ без Плюса полные треки недоступны — ниже, скорее всего, будет ошибка");
    }

    println!("2/5 поиск «{query}»…");
    let tracks = api.search_tracks(query, 3).await?;
    if tracks.is_empty() {
        bail!("поиск ничего не вернул — проверьте запрос");
    }
    for track in &tracks {
        println!(
            "    [{}] {} — {} ({}){}",
            track.id,
            track.artist_names(),
            track.title,
            fmt_ms(track.duration_ms),
            if track.available {
                ""
            } else {
                "  (недоступен)"
            }
        );
    }
    let track = &tracks[0];

    println!("3/5 ссылка на поток для [{}]…", track.id);
    let url = api.stream_url(&track.id.0).await?;
    println!("    подпись принята: {}", api::redact_url(&url));

    println!("4/5 загрузка…");
    let data = api.fetch_track(&url).await?;
    println!("    {:.1} МиБ", data.len() as f64 / (1024.0 * 1024.0));

    println!("5/5 декодирование…");
    match Player::probe_decode(data)? {
        Some(duration) => {
            let decoded_ms = duration.as_millis() as u64;
            println!(
                "    длительность {} (в метаданных {})",
                fmt_ms(decoded_ms),
                fmt_ms(track.duration_ms)
            );
            let skew = decoded_ms.abs_diff(track.duration_ms);
            if track.duration_ms > 0 && skew > 5_000 {
                println!(
                    "    ⚠ расхождение {} — возможно, пришёл фрагмент, а не полный трек",
                    fmt_ms(skew)
                );
            }
        }
        None => println!("    ⚠ декодер не сообщил длительность (переход может быть неточным)"),
    }

    println!();
    println!("готово: токен, поиск, подпись ссылки, загрузка и декодирование работают.");
    Ok(())
}

async fn search(cfg: &Config, query: &str, limit: usize) -> Result<()> {
    if query.trim().is_empty() {
        bail!("пустой поисковый запрос");
    }
    let api = YandexMusic::new(cfg.require_yandex_token()?)?;
    let tracks = api.search_tracks(query, limit).await?;
    if tracks.is_empty() {
        println!("ничего не найдено");
        return Ok(());
    }
    for (index, track) in tracks.iter().enumerate() {
        println!(
            "{:2}. {:>12}  {:>6}  {} — {}{}{}",
            index + 1,
            track.id,
            fmt_ms(track.duration_ms),
            track.artist_names(),
            track.title,
            track
                .album_id()
                .map_or_else(String::new, |id| format!("  (альбом {id})")),
            if track.available {
                ""
            } else {
                "  (недоступен)"
            }
        );
    }
    println!();
    println!("запуск: ymsync play --track <id>  ·  весь альбом: ymsync play --album <id>");
    Ok(())
}

fn present(yes: bool) -> &'static str {
    if yes { "есть" } else { "нет" }
}

fn opt(value: Option<i64>) -> String {
    value.map_or_else(|| "?".to_string(), |v| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Notepad saves UTF-8 with a BOM, so `ymsync master … < commands.txt` used
    /// to silently reject its very first line.
    #[test]
    fn a_leading_byte_order_mark_is_ignored() {
        assert_eq!(
            parse_input("\u{feff}p"),
            Action::Engine(EngineCommand::TogglePause)
        );
        assert_eq!(parse_input("\u{feff}q"), Action::Quit);
    }

    #[test]
    fn windows_line_endings_and_padding_are_ignored() {
        assert_eq!(
            parse_input("  p \r"),
            Action::Engine(EngineCommand::TogglePause)
        );
        assert_eq!(parse_input("\tq\r\n"), Action::Quit);
    }

    #[test]
    fn simple_commands_parse() {
        assert_eq!(parse_input("q"), Action::Quit);
        assert_eq!(parse_input("exit"), Action::Quit);
        assert_eq!(parse_input("?"), Action::Help);
        assert_eq!(parse_input(""), Action::Status);
        assert_eq!(parse_input("status"), Action::Status);
        assert_eq!(
            parse_input("pause"),
            Action::Engine(EngineCommand::TogglePause)
        );
        assert_eq!(parse_input("n"), Action::Engine(EngineCommand::Next));
        assert_eq!(parse_input("next"), Action::Engine(EngineCommand::Next));
        assert_eq!(parse_input("b"), Action::Engine(EngineCommand::Prev));
    }

    #[test]
    fn seeks_parse_as_absolute_or_relative() {
        assert_eq!(
            parse_input("120"),
            Action::Engine(EngineCommand::SeekTo(120_000))
        );
        assert_eq!(parse_input("0"), Action::Engine(EngineCommand::SeekTo(0)));
        assert_eq!(
            parse_input("12.5"),
            Action::Engine(EngineCommand::SeekTo(12_500))
        );
        assert_eq!(
            parse_input("+30"),
            Action::Engine(EngineCommand::SeekBy(30_000))
        );
        assert_eq!(
            parse_input("-10"),
            Action::Engine(EngineCommand::SeekBy(-10_000))
        );
    }

    #[test]
    fn queue_entries_are_selected_one_based() {
        assert_eq!(
            parse_input("#1"),
            Action::Engine(EngineCommand::PlayIndex(0))
        );
        assert_eq!(
            parse_input("#12"),
            Action::Engine(EngineCommand::PlayIndex(11))
        );
        assert_eq!(parse_input("#0"), Action::Unknown("#0".to_string()));
        assert_eq!(parse_input("#x"), Action::Unknown("#x".to_string()));
    }

    #[test]
    fn volume_parses_as_a_percentage_and_clamps() {
        assert_eq!(
            parse_input("v 70"),
            Action::Engine(EngineCommand::SetVolume(0.7))
        );
        assert_eq!(
            parse_input("v 900"),
            Action::Engine(EngineCommand::SetVolume(2.0))
        );
        assert_eq!(
            parse_input("v -5"),
            Action::Engine(EngineCommand::SetVolume(0.0))
        );
    }

    #[test]
    fn nonsense_is_reported_rather_than_acted_on() {
        assert_eq!(parse_input("хм"), Action::Unknown("хм".to_string()));
        assert_eq!(parse_input("v abc"), Action::Unknown("v abc".to_string()));
        assert_eq!(parse_input("+abc"), Action::Unknown("+abc".to_string()));
        assert_eq!(parse_input("nan"), Action::Unknown("nan".to_string()));
        assert_eq!(parse_input("inf"), Action::Unknown("inf".to_string()));
    }
}
