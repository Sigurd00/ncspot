use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use dbus::arg::{RefArg, Variant};
use dbus::ffidisp::stdintf::org_freedesktop_dbus::PropertiesPropertiesChanged;
use dbus::message::SignalArgs;
use dbus::strings::Path;
use dbus_tree::{Access, Factory};
use log::{debug, warn};

use crate::events::EventManager;
use crate::library::Library;
use crate::model::album::Album;
use crate::model::episode::Episode;
use crate::model::playable::Playable;
use crate::model::playlist::Playlist;
use crate::model::show::Show;
use crate::model::track::Track;
use crate::queue::{Queue, RepeatSetting};
use crate::spotify::{PlayerEvent, Spotify, UriType, VOLUME_PERCENT};
use crate::traits::ListItem;
use regex::Regex;

type Metadata = HashMap<String, Variant<Box<dyn RefArg>>>;

struct MprisState(String, Option<Playable>);

fn get_playbackstatus(spotify: Spotify) -> String {
    match spotify.get_current_status() {
        PlayerEvent::Playing(_) | PlayerEvent::FinishedTrack => "Playing",
        PlayerEvent::Paused(_) => "Paused",
        _ => "Stopped",
    }
    .to_string()
}

fn get_metadata(playable: Option<Playable>, spotify: Spotify, library: Arc<Library>) -> Metadata {
    let mut hm: Metadata = HashMap::new();

    // Fetch full track details in case this playable is based on a SimplifiedTrack
    // This is necessary because SimplifiedTrack objects don't contain a cover_url
    let playable_full = playable.and_then(|p| match p {
        Playable::Track(track) => {
            if track.cover_url.is_some() {
                // We already have `cover_url`, no need to fetch the full track
                Some(Playable::Track(track))
            } else {
                spotify
                    .api
                    .track(&track.id.unwrap_or_default())
                    .as_ref()
                    .map(|t| Playable::Track(t.into()))
            }
        }
        Playable::Episode(episode) => Some(Playable::Episode(episode)),
    });
    let playable = playable_full.as_ref();

    hm.insert(
        "mpris:trackid".to_string(),
        Variant(Box::new(Path::from(format!(
            "/org/ncspot/{}",
            playable
                .filter(|t| t.id().is_some())
                .map(|t| t.uri().replace(':', "/"))
                .unwrap_or_else(|| String::from("0"))
        )))),
    );
    hm.insert(
        "mpris:length".to_string(),
        Variant(Box::new(
            playable.map(|t| t.duration() as i64 * 1_000).unwrap_or(0),
        )),
    );
    hm.insert(
        "mpris:artUrl".to_string(),
        Variant(Box::new(
            playable
                .map(|t| t.cover_url().unwrap_or_default())
                .unwrap_or_default(),
        )),
    );

    hm.insert(
        "xesam:album".to_string(),
        Variant(Box::new(
            playable
                .and_then(|p| p.track())
                .map(|t| t.album.unwrap_or_default())
                .unwrap_or_default(),
        )),
    );
    hm.insert(
        "xesam:albumArtist".to_string(),
        Variant(Box::new(
            playable
                .and_then(|p| p.track())
                .map(|t| t.album_artists)
                .unwrap_or_default(),
        )),
    );
    hm.insert(
        "xesam:artist".to_string(),
        Variant(Box::new(
            playable
                .and_then(|p| p.track())
                .map(|t| t.artists)
                .unwrap_or_default(),
        )),
    );
    hm.insert(
        "xesam:discNumber".to_string(),
        Variant(Box::new(
            playable
                .and_then(|p| p.track())
                .map(|t| t.disc_number)
                .unwrap_or(0),
        )),
    );
    hm.insert(
        "xesam:title".to_string(),
        Variant(Box::new(
            playable
                .map(|t| match t {
                    Playable::Track(t) => t.title.clone(),
                    Playable::Episode(ep) => ep.name.clone(),
                })
                .unwrap_or_default(),
        )),
    );
    hm.insert(
        "xesam:trackNumber".to_string(),
        Variant(Box::new(
            playable
                .and_then(|p| p.track())
                .map(|t| t.track_number)
                .unwrap_or(0) as i32,
        )),
    );
    hm.insert(
        "xesam:url".to_string(),
        Variant(Box::new(
            playable
                .map(|t| t.share_url().unwrap_or_default())
                .unwrap_or_default(),
        )),
    );
    hm.insert(
        "xesam:userRating".to_string(),
        Variant(Box::new(
            playable
                .and_then(|p| p.track())
                .map(|t| match library.is_saved_track(&Playable::Track(t)) {
                    true => 1.0,
                    false => 0.0,
                })
                .unwrap_or(0.0) as f64,
        )),
    );

    hm
}

fn run_dbus_server(
    ev: EventManager,
    spotify: Spotify,
    queue: Arc<Queue>,
    library: Arc<Library>,
    rx: mpsc::Receiver<MprisState>,
) {
    let conn = Rc::new(
        dbus::ffidisp::Connection::get_private(dbus::ffidisp::BusType::Session)
            .expect("Failed to connect to dbus"),
    );
    conn.register_name(
        "org.mpris.MediaPlayer2.ncspot",
        dbus::ffidisp::NameFlag::ReplaceExisting as u32,
    )
    .expect("Failed to register dbus player name");

    let f = Factory::new_fn::<()>();

    let property_canquit = f
        .property::<bool, _>("CanQuit", ())
        .access(Access::Read)
        .on_get(|iter, _| {
            iter.append(false); // TODO
            Ok(())
        });

    let property_canraise = f
        .property::<bool, _>("CanRaise", ())
        .access(Access::Read)
        .on_get(|iter, _| {
            iter.append(false);
            Ok(())
        });

    let property_cansetfullscreen = f
        .property::<bool, _>("CanSetFullscreen", ())
        .access(Access::Read)
        .on_get(|iter, _| {
            iter.append(false);
            Ok(())
        });

    let property_hastracklist = f
        .property::<bool, _>("HasTrackList", ())
        .access(Access::Read)
        .on_get(|iter, _| {
            iter.append(false); // TODO
            Ok(())
        });

    let property_identity = f
        .property::<String, _>("Identity", ())
        .access(Access::Read)
        .on_get(|iter, _| {
            iter.append("ncspot".to_string());
            Ok(())
        });

    let property_urischemes = f
        .property::<Vec<String>, _>("SupportedUriSchemes", ())
        .access(Access::Read)
        .on_get(|iter, _| {
            iter.append(vec!["spotify".to_string()]);
            Ok(())
        });

    let property_mimetypes = f
        .property::<Vec<String>, _>("SupportedMimeTypes", ())
        .access(Access::Read)
        .on_get(|iter, _| {
            iter.append(Vec::new() as Vec<String>);
            Ok(())
        });

    // https://specifications.freedesktop.org/mpris-spec/latest/Media_Player.html
    let interface = f
        .interface("org.mpris.MediaPlayer2", ())
        .add_p(property_canquit)
        .add_p(property_canraise)
        .add_p(property_cansetfullscreen)
        .add_p(property_hastracklist)
        .add_p(property_identity)
        .add_p(property_urischemes)
        .add_p(property_mimetypes);

    let property_playbackstatus = {
        let spotify = spotify.clone();
        f.property::<String, _>("PlaybackStatus", ())
            .access(Access::Read)
            .on_get(move |iter, _| {
                let status = get_playbackstatus(spotify.clone());
                iter.append(status);
                Ok(())
            })
    };

    let property_loopstatus = {
        let queue1 = queue.clone();
        let queue2 = queue.clone();
        f.property::<String, _>("LoopStatus", ())
            .access(Access::ReadWrite)
            .on_get(move |iter, _| {
                iter.append(
                    match queue1.get_repeat() {
                        RepeatSetting::None => "None",
                        RepeatSetting::RepeatTrack => "Track",
                        RepeatSetting::RepeatPlaylist => "Playlist",
                    }
                    .to_string(),
                );
                Ok(())
            })
            .on_set(move |iter, _| {
                let setting = match iter.get::<&str>().unwrap_or_default() {
                    "Track" => RepeatSetting::RepeatTrack,
                    "Playlist" => RepeatSetting::RepeatPlaylist,
                    _ => RepeatSetting::None,
                };
                queue2.set_repeat(setting);

                Ok(())
            })
    };

    let property_metadata = {
        let spotify = spotify.clone();
        let queue = queue.clone();
        let library = library.clone();
        f.property::<HashMap<String, Variant<Box<dyn RefArg>>>, _>("Metadata", ())
            .access(Access::Read)
            .on_get(move |iter, _| {
                let hm = get_metadata(
                    queue.clone().get_current(),
                    spotify.clone(),
                    library.clone(),
                );

                iter.append(hm);
                Ok(())
            })
    };

    let property_position = {
        let spotify = spotify.clone();
        f.property::<i64, _>("Position", ())
            .access(Access::Read)
            .on_get(move |iter, _| {
                let progress = spotify.get_current_progress();
                iter.append(progress.as_micros() as i64);
                Ok(())
            })
    };

    let property_volume = {
        let spotify1 = spotify.clone();
        let spotify2 = spotify.clone();
        let event = ev.clone();
        f.property::<f64, _>("Volume", ())
            .access(Access::ReadWrite)
            .on_get(move |i, _| {
                i.append(spotify1.volume() as f64 / 65535_f64);
                Ok(())
            })
            .on_set(move |i, _| {
                let cur = spotify2.volume() as f64 / 65535_f64;
                let req = i.get::<f64>().unwrap_or(cur);
                if (0.0..=1.0).contains(&req) {
                    let vol = (VOLUME_PERCENT as f64) * req * 100.0;
                    spotify2.set_volume(vol as u16);
                }
                event.trigger();
                Ok(())
            })
    };

    let property_rate = f
        .property::<f64, _>("Rate", ())
        .access(Access::Read)
        .on_get(|iter, _| {
            iter.append(1.0);
            Ok(())
        });

    let property_minrate = f
        .property::<f64, _>("MinimumRate", ())
        .access(Access::Read)
        .on_get(|iter, _| {
            iter.append(1.0);
            Ok(())
        });

    let property_maxrate = f
        .property::<f64, _>("MaximumRate", ())
        .access(Access::Read)
        .on_get(|iter, _| {
            iter.append(1.0);
            Ok(())
        });

    let property_canplay = f
        .property::<bool, _>("CanPlay", ())
        .access(Access::Read)
        .on_get(|iter, _| {
            iter.append(true);
            Ok(())
        });

    let property_canpause = f
        .property::<bool, _>("CanPause", ())
        .access(Access::Read)
        .on_get(|iter, _| {
            iter.append(true);
            Ok(())
        });

    let property_canseek = f
        .property::<bool, _>("CanSeek", ())
        .access(Access::Read)
        .on_get(|iter, _| {
            iter.append(true);
            Ok(())
        });

    let property_cancontrol = f
        .property::<bool, _>("CanControl", ())
        .access(Access::Read)
        .on_get(|iter, _| {
            iter.append(true);
            Ok(())
        });

    let property_cangonext = f
        .property::<bool, _>("CanGoNext", ())
        .access(Access::Read)
        .on_get(|iter, _| {
            iter.append(true);
            Ok(())
        });

    let property_cangoprevious = f
        .property::<bool, _>("CanGoPrevious", ())
        .access(Access::Read)
        .on_get(|iter, _| {
            iter.append(true);
            Ok(())
        });

    let property_shuffle = {
        let queue_get = queue.clone();
        let queue_set = queue.clone();
        f.property::<bool, _>("Shuffle", ())
            .access(Access::ReadWrite)
            .on_get(move |iter, _| {
                let current_state = queue_get.get_shuffle();
                iter.append(current_state);
                Ok(())
            })
            .on_set(move |iter, _| {
                if let Some(shuffle_state) = iter.get() {
                    queue_set.set_shuffle(shuffle_state);
                }
                ev.trigger();
                Ok(())
            })
    };

    let property_cangoforward = f
        .property::<bool, _>("CanGoForward", ())
        .access(Access::Read)
        .on_get(|iter, _| {
            iter.append(true);
            Ok(())
        });

    let property_canrewind = f
        .property::<bool, _>("CanRewind", ())
        .access(Access::Read)
        .on_get(|iter, _| {
            iter.append(true);
            Ok(())
        });

    let method_playpause = {
        let queue = queue.clone();
        f.method("PlayPause", (), move |m| {
            queue.toggleplayback();
            Ok(vec![m.msg.method_return()])
        })
    };

    let method_play = {
        let spotify = spotify.clone();
        f.method("Play", (), move |m| {
            spotify.play();
            Ok(vec![m.msg.method_return()])
        })
    };

    let method_pause = {
        let spotify = spotify.clone();
        f.method("Pause", (), move |m| {
            spotify.pause();
            Ok(vec![m.msg.method_return()])
        })
    };

    let method_stop = {
        let spotify = spotify.clone();
        f.method("Stop", (), move |m| {
            spotify.stop();
            Ok(vec![m.msg.method_return()])
        })
    };

    let method_next = {
        let queue = queue.clone();
        f.method("Next", (), move |m| {
            queue.next(true);
            Ok(vec![m.msg.method_return()])
        })
    };

    let method_previous = {
        let spotify = spotify.clone();
        let queue = queue.clone();
        f.method("Previous", (), move |m| {
            if spotify.get_current_progress() < Duration::from_secs(5) {
                queue.previous();
            } else {
                spotify.seek(0);
            }
            Ok(vec![m.msg.method_return()])
        })
    };

    let method_forward = {
        let spotify = spotify.clone();
        f.method("Forward", (), move |m| {
            spotify.seek_relative(5000);
            Ok(vec![m.msg.method_return()])
        })
    };

    let method_rewind = {
        let spotify = spotify.clone();
        f.method("Rewind", (), move |m| {
            spotify.seek_relative(-5000);
            Ok(vec![m.msg.method_return()])
        })
    };

    let method_seek = {
        let queue = queue.clone();
        let spotify = spotify.clone();
        f.method("Seek", (), move |m| {
            if let Some(current_track) = queue.get_current() {
                let offset = m.msg.get1::<i64>().unwrap_or(0); // micros
                let progress = spotify.get_current_progress();
                let new_position = (progress.as_secs() * 1000) as i32
                    + progress.subsec_millis() as i32
                    + (offset / 1000) as i32;
                let new_position = new_position.max(0) as u32;
                let duration = current_track.duration();

                if new_position < duration {
                    spotify.seek(new_position);
                } else {
                    queue.next(true);
                }
            }
            Ok(vec![m.msg.method_return()])
        })
    };

    let method_set_position = {
        let queue = queue.clone();
        let spotify = spotify.clone();
        f.method("SetPosition", (), move |m| {
            if let Some(current_track) = queue.get_current() {
                let (_, position) = m.msg.get2::<Path, i64>(); // micros
                let position = (position.unwrap_or(0) / 1000) as u32;
                let duration = current_track.duration();

                if position < duration {
                    spotify.seek(position);
                }
            }
            Ok(vec![m.msg.method_return()])
        })
    };

    let method_openuri = {
        let spotify = spotify.clone();
        f.method("OpenUri", (), move |m| {
            let uri_data: Option<&str> = m.msg.get1();
            let uri = match uri_data {
                Some(s) => {
                    let spotify_uri = if s.contains("open.spotify.com") {
                        let regex = Regex::new(r"https?://open\.spotify\.com(/user/\S+)?/(album|track|playlist|show|episode)/(.+)(\?si=\S+)?").unwrap();
                        let captures = regex.captures(s).unwrap();
                        let uri_type = &captures[2];
                        let id = &captures[3];
                        format!("spotify:{}:{}", uri_type, id)
                    }else {
                        s.to_string()
                    };
                    spotify_uri
                }
                None => "".to_string(),
            };
            let id = &uri[uri.rfind(':').unwrap_or(0) + 1..uri.len()];
            let uri_type = UriType::from_uri(&uri);
            match uri_type {
                Some(UriType::Album) => {
                    if let Some(a) = spotify.api.album(id) {
                        if let Some(t) = &Album::from(&a).tracks {
                            let should_shuffle = queue.get_shuffle();
                            queue.clear();
                            let index = queue.append_next(
                                &t.iter()
                                    .map(|track| Playable::Track(track.clone()))
                                    .collect(),
                            );
                            queue.play(index, should_shuffle, should_shuffle)
                        }
                    }
                }
                Some(UriType::Track) => {
                    if let Some(t) = spotify.api.track(id) {
                        queue.clear();
                        queue.append(Playable::Track(Track::from(&t)));
                        queue.play(0, false, false)
                    }
                }
                Some(UriType::Playlist) => {
                    if let Some(p) = spotify.api.playlist(id) {
                        let mut playlist = Playlist::from(&p);
                        let spotify = spotify.clone();
                        playlist.load_tracks(spotify);
                        if let Some(tracks) = &playlist.tracks {
                            let should_shuffle = queue.get_shuffle();
                            queue.clear();
                            let index = queue.append_next(tracks);
                            queue.play(index, should_shuffle, should_shuffle)
                        }
                    }
                }
                Some(UriType::Show) => {
                    if let Some(s) = spotify.api.get_show(id) {
                        let mut show: Show = (&s).into();
                        let spotify = spotify.clone();
                        show.load_all_episodes(spotify);
                        if let Some(e) = &show.episodes {
                            let should_shuffle = queue.get_shuffle();
                            queue.clear();
                            let mut ep = e.clone();
                            ep.reverse();
                            let index = queue.append_next(
                                &ep.iter()
                                    .map(|episode| Playable::Episode(episode.clone()))
                                    .collect(),
                            );
                            queue.play(index, should_shuffle, should_shuffle)
                        }
                    }
                }
                Some(UriType::Episode) => {
                    if let Some(e) = spotify.api.episode(id) {
                        queue.clear();
                        queue.append(Playable::Episode(Episode::from(&e)));
                        queue.play(0, false, false)
                    }
                }
                Some(UriType::Artist) => {
                    if let Some(a) = spotify.api.artist_top_tracks(id) {
                        let should_shuffle = queue.get_shuffle();
                        queue.clear();
                        let index = queue.append_next(&a.iter().map(|track| Playable::Track(track.clone())).collect());
                        queue.play(index, should_shuffle, should_shuffle)
                    }
                }
                None => {}
            }
            Ok(vec![m.msg.method_return()])
        })
    };

    // https://specifications.freedesktop.org/mpris-spec/latest/Player_Interface.html
    let interface_player = f
        .interface("org.mpris.MediaPlayer2.Player", ())
        .add_p(property_playbackstatus)
        .add_p(property_loopstatus)
        .add_p(property_metadata)
        .add_p(property_position)
        .add_p(property_volume)
        .add_p(property_rate)
        .add_p(property_minrate)
        .add_p(property_maxrate)
        .add_p(property_canplay)
        .add_p(property_canpause)
        .add_p(property_canseek)
        .add_p(property_cancontrol)
        .add_p(property_cangonext)
        .add_p(property_cangoprevious)
        .add_p(property_shuffle)
        .add_p(property_cangoforward)
        .add_p(property_canrewind)
        .add_m(method_playpause)
        .add_m(method_play)
        .add_m(method_pause)
        .add_m(method_stop)
        .add_m(method_next)
        .add_m(method_previous)
        .add_m(method_forward)
        .add_m(method_rewind)
        .add_m(method_seek)
        .add_m(method_set_position)
        .add_m(method_openuri);

    let tree = f.tree(()).add(
        f.object_path("/org/mpris/MediaPlayer2", ())
            .introspectable()
            .add(interface)
            .add(interface_player),
    );

    tree.set_registered(&conn, true)
        .expect("failed to register tree");

    conn.add_handler(tree);
    loop {
        if let Some(m) = conn.incoming(200).next() {
            warn!("Unhandled dbus message: {:?}", m);
        }

        if let Ok(state) = rx.try_recv() {
            let mut changed: PropertiesPropertiesChanged = Default::default();
            debug!(
                "mpris PropertiesChanged: status {}, track: {:?}",
                state.0, state.1
            );

            changed.interface_name = "org.mpris.MediaPlayer2.Player".to_string();
            changed.changed_properties.insert(
                "Metadata".to_string(),
                Variant(Box::new(get_metadata(
                    state.1,
                    spotify.clone(),
                    library.clone(),
                ))),
            );

            changed
                .changed_properties
                .insert("PlaybackStatus".to_string(), Variant(Box::new(state.0)));

            conn.send(
                changed.to_emit_message(&Path::new("/org/mpris/MediaPlayer2".to_string()).unwrap()),
            )
            .unwrap();
        }
    }
}

#[derive(Clone)]
pub struct MprisManager {
    tx: mpsc::Sender<MprisState>,
    queue: Arc<Queue>,
    spotify: Spotify,
}

impl MprisManager {
    pub fn new(
        ev: EventManager,
        spotify: Spotify,
        queue: Arc<Queue>,
        library: Arc<Library>,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<MprisState>();

        {
            let spotify = spotify.clone();
            let queue = queue.clone();
            std::thread::spawn(move || {
                run_dbus_server(ev, spotify.clone(), queue.clone(), library.clone(), rx);
            });
        }

        MprisManager { tx, queue, spotify }
    }

    pub fn update(&self) {
        let status = get_playbackstatus(self.spotify.clone());
        let track = self.queue.get_current();
        self.tx.send(MprisState(status, track)).unwrap();
    }
}
