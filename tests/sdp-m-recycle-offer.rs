use std::net::Ipv4Addr;
use std::time::Duration;
use std::time::Instant;

mod common;
use common::TestRtc;
use common::init_crypto_default;
use common::init_log;
use common::negotiate;
use common::progress;
use str0m::Event;
use str0m::Rtc;
use str0m::RtcError;
use str0m::media::Direction;
use str0m::media::MediaKind;
use str0m::media::Mid;
use tracing::Span;
use tracing::info_span;

// Offers made by str0m itself recycle the slot of a stopped m-line (RFC 8829 section 5.2.2)
// when `RtcConfig::set_recycle_stopped_mlines` is on, so a session that keeps adding and
// stopping media keeps its SDP bounded.

#[test]
pub fn offer_recycles_a_negotiated_stopped_slot() {
    init_log();
    init_crypto_default();

    let mut l = peer(info_span!("L"), true);
    let mut r = peer(info_span!("R"), false);

    let old_mid = negotiate(&mut l, &mut r, |change| {
        change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None)
    });
    negotiate(&mut l, &mut r, |change| change.stop_media(old_mid));

    let (offer, pending, new_mid) = l.span.in_scope(|| {
        let mut change = l.rtc.sdp_api();
        let mid = change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
        let (offer, pending) = change.apply().unwrap();
        (offer, pending, mid)
    });
    let offer_sdp = offer.to_sdp_string();
    assert_eq!(
        m_lines(&offer_sdp),
        1,
        "the new m-line takes the stopped slot:\n{offer_sdp}"
    );
    assert!(
        !has_mid(&offer_sdp, old_mid),
        "the retired mid is gone:\n{offer_sdp}"
    );
    assert!(
        bundle(&offer_sdp).contains(&new_mid.to_string()),
        "{offer_sdp}"
    );

    let answer = r
        .span
        .in_scope(|| r.rtc.sdp_api().accept_offer(offer).unwrap());
    assert_eq!(m_lines(&answer.to_sdp_string()), 1);
    l.span
        .in_scope(|| l.rtc.sdp_api().accept_answer(pending, answer))
        .expect("the answer confirms the recycled slot");

    for (side, rtc) in [("L", &l), ("R", &r)] {
        assert!(rtc.media(old_mid).is_none(), "{side} retired the old mid");
        let media = rtc.media(new_mid).expect("new mid present");
        assert!(!media.stopped(), "{side}: the recycled m-line is live");
        assert_eq!(media.kind(), MediaKind::Video);
    }
    assert_eq!(l.media(new_mid).unwrap().direction(), Direction::SendOnly);
    assert_eq!(r.media(new_mid).unwrap().direction(), Direction::RecvOnly);
}

#[test]
pub fn a_slot_is_not_recycled_before_its_stop_is_negotiated() {
    init_log();
    init_crypto_default();

    let mut l = peer(info_span!("L"), true);
    let mut r = peer(info_span!("R"), false);

    let old_mid = negotiate(&mut l, &mut r, |change| {
        change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None)
    });

    // Stopped and added in one offer: the remote has not agreed to the stop yet, so the
    // stopped m-line keeps its slot (port 0) and the new one is appended.
    let offer = l.span.in_scope(|| {
        let mut change = l.rtc.sdp_api();
        change.stop_media(old_mid);
        change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
        change.apply().unwrap().0
    });
    let sdp = offer.to_sdp_string();
    assert_eq!(m_lines(&sdp), 2, "{sdp}");
    assert!(has_mid(&sdp, old_mid), "{sdp}");
}

#[test]
pub fn a_slot_is_not_recycled_while_the_stop_awaits_its_answer() {
    init_log();
    init_crypto_default();

    let mut l = peer(info_span!("L"), true);
    let mut r = peer(info_span!("R"), false);

    let old_mid = negotiate(&mut l, &mut r, |change| {
        change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None)
    });

    // The stop's offer is made but never answered; a merged follow-up offer still appends.
    let pending = l.span.in_scope(|| {
        let mut change = l.rtc.sdp_api();
        change.stop_media(old_mid);
        change.apply().unwrap().1
    });
    let offer = l.span.in_scope(|| {
        let mut change = l.rtc.sdp_api();
        change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
        change.merge(pending);
        change.apply().unwrap().0
    });
    let sdp = offer.to_sdp_string();
    assert_eq!(m_lines(&sdp), 2, "{sdp}");

    let answer = r
        .span
        .in_scope(|| r.rtc.sdp_api().accept_offer(offer).unwrap());
    assert_eq!(m_lines(&answer.to_sdp_string()), 2);
}

#[test]
pub fn recycling_is_off_by_default() {
    init_log();
    init_crypto_default();

    assert!(!Rtc::builder().recycle_stopped_mlines());

    let mut l = peer(info_span!("L"), false);
    let mut r = peer(info_span!("R"), false);

    let old_mid = negotiate(&mut l, &mut r, |change| {
        change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None)
    });
    negotiate(&mut l, &mut r, |change| change.stop_media(old_mid));

    let offer = l.span.in_scope(|| {
        let mut change = l.rtc.sdp_api();
        change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
        change.apply().unwrap().0
    });
    let sdp = offer.to_sdp_string();
    assert_eq!(m_lines(&sdp), 2, "{sdp}");
    assert!(has_mid(&sdp, old_mid), "{sdp}");
}

#[test]
pub fn a_recycled_slot_may_change_media_kind_and_lowest_slots_go_first() {
    init_log();
    init_crypto_default();

    let mut l = peer(info_span!("L"), true);
    let mut r = peer(info_span!("R"), false);

    let [a, b, c] = negotiate(&mut l, &mut r, |change| {
        [MediaKind::Audio, MediaKind::Video, MediaKind::Video]
            .map(|kind| change.add_media(kind, Direction::SendOnly, None, None, None))
    });
    // Stop the first and the last, keep the middle one.
    negotiate(&mut l, &mut r, |change| {
        change.stop_media(a);
        change.stop_media(c);
    });

    let [x, y, z] = negotiate(&mut l, &mut r, |change| {
        [MediaKind::Video, MediaKind::Audio, MediaKind::Video]
            .map(|kind| change.add_media(kind, Direction::SendOnly, None, None, None))
    });

    for rtc in [&l, &r] {
        assert!(rtc.media(a).is_none() && rtc.media(c).is_none());
        assert_eq!(
            index_of(rtc, x),
            0,
            "lowest stopped slot first, audio slot now video"
        );
        assert_eq!(index_of(rtc, b), 1, "a live m-line keeps its slot");
        assert_eq!(index_of(rtc, y), 2);
        assert_eq!(index_of(rtc, z), 3, "no slot left, appended");
        assert_eq!(rtc.media(x).unwrap().kind(), MediaKind::Video);
        assert_eq!(rtc.media(y).unwrap().kind(), MediaKind::Audio);
    }
}

#[test]
pub fn churn_keeps_the_sdp_bounded() {
    init_log();
    init_crypto_default();

    let mut l = peer(info_span!("L"), true);
    let mut r = peer(info_span!("R"), false);

    // An SFU's subscriber connection: one m-line stays, others come and go.
    negotiate(&mut l, &mut r, |change| {
        change.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None)
    });
    let mut first_len = None;
    for _ in 0..50 {
        let mids = negotiate(&mut l, &mut r, |change| {
            [MediaKind::Video, MediaKind::Audio]
                .map(|kind| change.add_media(kind, Direction::SendOnly, None, None, None))
        });
        negotiate(&mut l, &mut r, |change| {
            mids.map(|mid| change.stop_media(mid))
        });

        let sdp = l.span.in_scope(|| {
            let mut change = l.rtc.sdp_api();
            change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
            change.apply().unwrap().0.to_sdp_string()
        });
        assert!(m_lines(&sdp) <= 3, "{sdp}");
        let len = *first_len.get_or_insert(sdp.len());
        assert!(
            sdp.len() < len + 256,
            "SDP grew from {len} to {} bytes",
            sdp.len()
        );
    }
    assert!(l.media_count() <= 3 && r.media_count() <= 3);
}

#[test]
pub fn media_flows_on_a_recycled_slot() -> Result<(), RtcError> {
    init_log();
    init_crypto_default();

    let mut l = peer(info_span!("L"), true);
    let mut r = peer(info_span!("R"), false);
    l.add_host_candidate((Ipv4Addr::new(1, 1, 1, 1), 1000).into());
    r.add_host_candidate((Ipv4Addr::new(2, 2, 2, 2), 2000).into());

    let old_mid = negotiate(&mut l, &mut r, |change| {
        change.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None)
    });
    while !(l.is_connected() && r.is_connected()) {
        progress(&mut l, &mut r)?;
    }
    negotiate(&mut l, &mut r, |change| change.stop_media(old_mid));
    let mid = negotiate(&mut l, &mut r, |change| {
        change.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None)
    });
    assert_eq!(index_of(&l, mid), 0);

    let pt = l.params_opus().pt();
    for _ in 0..20 {
        let wallclock = l.start + l.duration();
        let time = l.duration().into();
        l.writer(mid)
            .unwrap()
            .write(pt, wallclock, time, vec![1_u8; 80])?;
        progress(&mut l, &mut r)?;
    }
    for _ in 0..20 {
        progress(&mut l, &mut r)?;
    }

    let received: Vec<Mid> = r
        .events
        .iter()
        .filter_map(|(_, e)| match e {
            Event::MediaData(d) => Some(d.mid),
            _ => None,
        })
        .collect();
    assert!(!received.is_empty(), "media arrived on the recycled slot");
    assert!(received.iter().all(|m| *m == mid));
    Ok(())
}

fn peer(span: Span, recycle: bool) -> TestRtc {
    let rtc = Rtc::builder()
        .set_recycle_stopped_mlines(recycle)
        .build(Instant::now());
    let mut peer = TestRtc::new_with_rtc(span, rtc);
    peer.set_forced_time_advance(Duration::from_millis(1));
    peer
}

trait MediaCount {
    fn media_count(&self) -> usize;
}

impl MediaCount for TestRtc {
    fn media_count(&self) -> usize {
        self._mids().len()
    }
}

fn index_of(rtc: &TestRtc, mid: Mid) -> usize {
    rtc._index_of(mid).expect("mid present")
}

fn m_lines(sdp: &str) -> usize {
    sdp.lines().filter(|l| l.starts_with("m=")).count()
}

fn has_mid(sdp: &str, mid: Mid) -> bool {
    let mid = mid.to_string();
    sdp.lines()
        .any(|l| l.strip_prefix("a=mid:").map(str::trim) == Some(mid.as_str()))
}

fn bundle(sdp: &str) -> Vec<String> {
    sdp.lines()
        .find_map(|l| l.strip_prefix("a=group:BUNDLE "))
        .map(|rest| rest.split_whitespace().map(String::from).collect())
        .unwrap_or_default()
}

#[test]
pub fn a_slot_the_remote_stopped_is_recycled_too() {
    init_log();
    init_crypto_default();

    let mut l = peer(info_span!("L"), true);
    let mut r = peer(info_span!("R"), false);

    let old_mid = negotiate(&mut l, &mut r, |change| {
        change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None)
    });
    // R stops it in an offer of its own; L's answer completes the stop.
    negotiate(&mut r, &mut l, |change| change.stop_media(old_mid));

    let new_mid = negotiate(&mut l, &mut r, |change| {
        change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None)
    });
    assert!(l.media(old_mid).is_none() && r.media(old_mid).is_none());
    assert_eq!(index_of(&l, new_mid), 0);
    assert_eq!(index_of(&r, new_mid), 0);
}

#[test]
pub fn an_answer_that_moves_a_recycled_m_line_is_rejected() {
    init_log();
    init_crypto_default();

    let mut l = peer(info_span!("L"), true);
    let mut r = peer(info_span!("R"), false);

    let keep = negotiate(&mut l, &mut r, |change| {
        change.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None)
    });
    let old_mid = negotiate(&mut l, &mut r, |change| {
        change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None)
    });
    negotiate(&mut l, &mut r, |change| change.stop_media(old_mid));

    let (offer, pending) = l.span.in_scope(|| {
        let mut change = l.rtc.sdp_api();
        change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
        change.apply().unwrap()
    });
    let answer = r
        .span
        .in_scope(|| r.rtc.sdp_api().accept_offer(offer).unwrap());

    // Swap the answer's two m-line sections: the recycled mid now sits at index 0.
    let sdp = answer.to_sdp_string();
    let mut sections: Vec<&str> = sdp.split("\r\nm=").collect();
    assert_eq!(sections.len(), 3, "{sdp}");
    sections.swap(1, 2);
    let swapped = sections.join("\r\nm=");
    let swapped = str0m::change::SdpAnswer::from_sdp_string(&swapped).expect("parses");

    let result = l
        .span
        .in_scope(|| l.rtc.sdp_api().accept_answer(pending, swapped));
    assert!(
        result.is_err(),
        "an answer must keep the offer's m-line order"
    );
    assert_eq!(index_of(&l, keep), 0);
}

#[test]
pub fn a_stop_completed_by_our_answer_frees_its_slot() {
    init_log();
    init_crypto_default();

    let mut l = peer(info_span!("L"), true);
    let mut r = peer(info_span!("R"), false);

    let old_mid = negotiate(&mut l, &mut r, |change| {
        change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None)
    });

    // L stops the m-line, but its offer is overtaken by one from R and dropped: the stop
    // reaches R as port 0 in L's answer instead.
    let _dropped = l.span.in_scope(|| {
        let mut change = l.rtc.sdp_api();
        change.stop_media(old_mid);
        change.apply().unwrap()
    });
    negotiate(&mut r, &mut l, |change| {
        change.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None)
    });
    assert!(
        r.media(old_mid).unwrap().stopped(),
        "R took the stop from the answer"
    );

    let new_mid = negotiate(&mut l, &mut r, |change| {
        change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None)
    });
    assert_eq!(index_of(&l, new_mid), 0);
    assert_eq!(index_of(&r, new_mid), 0);
}
