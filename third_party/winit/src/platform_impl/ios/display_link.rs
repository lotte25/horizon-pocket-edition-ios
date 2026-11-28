//! Horizon patch: ask Core Animation for ProMotion refresh rates.
//!
//! On ProMotion iPhones the `CADisableMinimumFrameDurationOnPhone` Info.plist key only *permits*
//! rates above 60 Hz. The display still runs at the rate that the app's display links ask for,
//! and winit drives its redraws from run-loop observers rather than from a display link, so a
//! Metal layer presenting as fast as it can stays at 60 Hz. Apple's guidance for full-motion
//! games is a `CADisplayLink` whose `preferredFrameRateRange` reaches 120 fps; osu! and Unity
//! do the same. This link does no work when it fires: it exists to state that frame rate, while
//! the game's own loop and frame limiter keep deciding when to draw and present.
//!
//! https://developer.apple.com/documentation/quartzcore/optimizing-promotion-refresh-rates-for-iphone-13-pro-and-ipad-pro

use std::cell::RefCell;

#[path = "display_link_policy.rs"]
mod policy;
use policy::Policy;

use core_foundation::runloop::kCFRunLoopCommonModes;
use objc2::encode::{Encode, Encoding};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{declare_class, msg_send, msg_send_id, mutability, sel, ClassType, DeclaredClass};
use objc2_foundation::{MainThreadMarker, NSObject};

/// `CAFrameRateRange` (iOS 15): three `float`s.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct CAFrameRateRange {
    minimum: f32,
    maximum: f32,
    preferred: f32,
}

unsafe impl Encode for CAFrameRateRange {
    const ENCODING: Encoding = Encoding::Struct(
        "CAFrameRateRange",
        &[f32::ENCODING, f32::ENCODING, f32::ENCODING],
    );
}

declare_class!(
    struct HorizonDisplayLinkTarget;

    unsafe impl ClassType for HorizonDisplayLinkTarget {
        type Super = NSObject;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "HorizonDisplayLinkTarget";
    }

    impl DeclaredClass for HorizonDisplayLinkTarget {}

    unsafe impl HorizonDisplayLinkTarget {
        #[method(tick:)]
        fn tick(&self, _link: &AnyObject) {}
    }
);

#[derive(Default)]
struct State {
    policy: Policy,
    link: Option<Retained<AnyObject>>,
    applied: Option<u32>,
}
thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// Called only on UIKit's main thread, through WindowExtIOS.
pub(crate) fn request(mtm: MainThreadMarker, requested: Option<u32>, maximum: u32) {
    STATE.with(|cell| {
        let mut state = cell.borrow_mut();
        state.policy.requested = requested;
        state.policy.maximum = maximum;
        apply(mtm, &mut state);
    });
}

/// Scene/application activation is independent from whether a game is running.
pub(crate) fn active(mtm: MainThreadMarker, active: bool) {
    STATE.with(|cell| {
        let mut state = cell.borrow_mut();
        state.policy.active = active;
        apply(mtm, &mut state);
    });
}

pub(crate) fn stop(_mtm: MainThreadMarker) {
    STATE.with(|cell| {
        let mut state = cell.borrow_mut();
        if let Some(link) = state.link.take() {
            let _: () = unsafe { msg_send![&*link, invalidate] };
        }
        *state = State::default();
    });
}

fn apply(mtm: MainThreadMarker, state: &mut State) {
    let rate = state.policy.rate();
    if rate == state.applied {
        return;
    }
    let Some(rate) = rate else {
        if let Some(link) = &state.link {
            let _: () = unsafe { msg_send![&**link, setPaused: true] };
        }
        state.applied = None;
        return;
    };
    if state.link.is_none() {
        state.link = create(mtm);
    }
    let Some(link) = &state.link else { return };
    let responds: bool =
        unsafe { msg_send![&**link, respondsToSelector: sel!(setPreferredFrameRateRange:)] };
    if responds {
        let range = CAFrameRateRange {
            minimum: rate.min(80) as f32,
            maximum: rate as f32,
            preferred: rate as f32,
        };
        let _: () = unsafe { msg_send![&**link, setPreferredFrameRateRange: range] };
    } else {
        let _: () = unsafe { msg_send![&**link, setPreferredFramesPerSecond: rate as isize] };
    }
    let _: () = unsafe { msg_send![&**link, setPaused: false] };
    state.applied = Some(rate);
}

/// Created lazily only after a visible, active game requests callbacks.
fn create(mtm: MainThreadMarker) -> Option<Retained<AnyObject>> {
    let (Some(display_link_class), Some(run_loop_class)) =
        (AnyClass::get("CADisplayLink"), AnyClass::get("NSRunLoop"))
    else {
        return None;
    };
    let target = mtm.alloc::<HorizonDisplayLinkTarget>().set_ivars(());
    let target: Retained<HorizonDisplayLinkTarget> = unsafe { msg_send_id![super(target), init] };
    let link: Option<Retained<AnyObject>> = unsafe {
        msg_send_id![display_link_class, displayLinkWithTarget: &*target, selector: sel!(tick:)]
    };
    let link = link?;
    let _: () = unsafe { msg_send![&*link, setPaused: true] };
    let run_loop: Retained<AnyObject> = unsafe { msg_send_id![run_loop_class, mainRunLoop] };
    let common_modes = unsafe { kCFRunLoopCommonModes } as *const AnyObject;
    let _: () = unsafe { msg_send![&*link, addToRunLoop: &*run_loop, forMode: common_modes] };
    Some(link)
}
