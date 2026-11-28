#![allow(clippy::unnecessary_cast)]
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2::{declare_class, msg_send, msg_send_id, mutability, sel, ClassType, DeclaredClass};
use objc2_foundation::{
    CGFloat, CGPoint, CGRect, CGSize, MainThreadMarker, NSObject, NSSet, NSString,
};
use objc2_ui_kit::{
    UICoordinateSpace, UIEditingInteractionConfiguration, UIEvent, UIEventType,
    UIForceTouchCapability, UIGestureRecognizer, UIGestureRecognizerDelegate,
    UIGestureRecognizerState, UIHoverGestureRecognizer, UIKeyInput, UIPanGestureRecognizer,
    UIPinchGestureRecognizer, UIPress, UIPressesEvent, UIResponder, UIRotationGestureRecognizer,
    UITapGestureRecognizer, UITextInputTraits, UITouch, UITouchPhase, UITouchType,
    UITraitEnvironment, UIView, UIWindow,
};

use super::app_state::{self, EventWrapper};
use super::window::WinitUIWindow;
use crate::dpi::PhysicalPosition;
use crate::event::{ElementState, Event, Force, KeyEvent, Touch, TouchPhase, WindowEvent};
use crate::keyboard::{
    Key, KeyCode, KeyLocation, ModifiersState, NamedKey, PhysicalKey,
};
use crate::platform_impl::platform::DEVICE_ID;
use crate::platform_impl::KeyEventExtra;
use crate::window::{WindowAttributes, WindowId as RootWindowId};

pub struct WinitViewState {
    pinch_gesture_recognizer: RefCell<Option<Retained<UIPinchGestureRecognizer>>>,
    doubletap_gesture_recognizer: RefCell<Option<Retained<UITapGestureRecognizer>>>,
    rotation_gesture_recognizer: RefCell<Option<Retained<UIRotationGestureRecognizer>>>,
    pan_gesture_recognizer: RefCell<Option<Retained<UIPanGestureRecognizer>>>,

    // for iOS delta references the start of the Gesture
    rotation_last_delta: Cell<CGFloat>,
    pinch_last_delta: Cell<CGFloat>,
    pan_last_delta: Cell<CGPoint>,
    ime_allowed: Cell<bool>,
    ime_view: Retained<WinitTextInputView>,
    held_keys: RefCell<HashMap<usize, KeyEvent>>,
    mouse: super::hardware_mouse::Mouse,
    /// Active direct-touch IDs -> last physical position, reconciled directly from
    /// `WinitUIWindow::sendEvent:` (or `UIEvent::touchesForView:` fallback) so system
    /// gesture recognizers cannot delay, swallow, or falsely cancel multi-touch gameplay.
    active_direct_touches: RefCell<HashMap<u64, PhysicalPosition<f64>>>,
    window_owns_direct_touches: Cell<bool>,
}

fn disable_recognizer_delays(view: &UIView) {
    if let Some(recognizers) = view.gestureRecognizers() {
        for gr in recognizers.iter() {
            let name = gr.class().name();
            if name.contains("SystemGesture")
                || name.contains("Editing")
                || name.contains("Scribble")
            {
                unsafe {
                    gr.setDelaysTouchesBegan(false);
                    gr.setDelaysTouchesEnded(false);
                }
            }
        }
    }
}

/// Disable `delaysTouchesBegan` and `delaysTouchesEnded` on UIKit's window-level
/// `_UISystemGestureGateGestureRecognizer` (and any system editing/scribble) instances so taps
/// and 3rd-finger presses near the bottom edge (D-pad Down, hotbar) reach the game immediately.
pub(crate) fn disable_system_gesture_delays(window: &UIWindow) {
    disable_recognizer_delays(window);
}

declare_class!(
    /// Dedicated invisible text-input responder kept as a 1x1 pass-through child of `WinitView`.
    /// Keeping `UIKeyInput` off `WinitView` prevents UIKit from activating 3-finger text-editing
    /// and selection gesture interceptors over the gameplay canvas while still allowing the iOS
    /// software keyboard to open whenever `ime_allowed` is true.
    pub(crate) struct WinitTextInputView;

    unsafe impl ClassType for WinitTextInputView {
        #[inherits(UIResponder, NSObject)]
        type Super = UIView;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "WinitTextInputView";
    }

    impl DeclaredClass for WinitTextInputView {}

    unsafe impl WinitTextInputView {
        #[method(pointInside:withEvent:)]
        fn point_inside(&self, _point: CGPoint, _event: Option<&UIEvent>) -> bool {
            false
        }

        #[method(canBecomeFirstResponder)]
        fn can_become_first_responder(&self) -> bool {
            self.parent_view()
                .is_some_and(|parent| parent.ivars().ime_allowed.get())
        }

        #[method(editingInteractionConfiguration)]
        fn editing_interaction_configuration(&self) -> UIEditingInteractionConfiguration {
            UIEditingInteractionConfiguration::None
        }

        #[method(resignFirstResponder)]
        fn resign_first_responder(&self) -> bool {
            if let Some(parent) = self.parent_view() {
                if parent.ivars().ime_allowed.get() {
                    parent.release_hardware_keys();
                }
            }
            unsafe { msg_send![super(self), resignFirstResponder] }
        }

        #[method(pressesBegan:withEvent:)]
        fn presses_began(&self, presses: &NSSet<UIPress>, event: Option<&UIPressesEvent>) {
            if !self
                .parent_view()
                .is_some_and(|p| p.handle_presses(presses, ElementState::Pressed))
            {
                let _: () = unsafe { msg_send![super(self), pressesBegan: presses withEvent: event] };
            }
        }

        #[method(pressesEnded:withEvent:)]
        fn presses_ended(&self, presses: &NSSet<UIPress>, event: Option<&UIPressesEvent>) {
            if !self
                .parent_view()
                .is_some_and(|p| p.handle_presses(presses, ElementState::Released))
            {
                let _: () = unsafe { msg_send![super(self), pressesEnded: presses withEvent: event] };
            }
        }

        #[method(pressesCancelled:withEvent:)]
        fn presses_cancelled(&self, presses: &NSSet<UIPress>, event: Option<&UIPressesEvent>) {
            if !self
                .parent_view()
                .is_some_and(|p| p.handle_presses(presses, ElementState::Released))
            {
                let _: () =
                    unsafe { msg_send![super(self), pressesCancelled: presses withEvent: event] };
            }
        }
    }

    unsafe impl NSObjectProtocol for WinitTextInputView {}

    unsafe impl UITextInputTraits for WinitTextInputView {}

    unsafe impl UIKeyInput for WinitTextInputView {
        #[method(hasText)]
        fn has_text(&self) -> bool {
            true
        }

        #[method(insertText:)]
        fn insert_text(&self, text: &NSString) {
            if let Some(parent) = self.parent_view() {
                parent.handle_insert_text(text);
            }
        }

        #[method(deleteBackward)]
        fn delete_backward(&self) {
            if let Some(parent) = self.parent_view() {
                parent.handle_delete_backward();
            }
        }
    }
);

impl WinitTextInputView {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let frame = CGRect {
            origin: CGPoint { x: 0.0, y: 0.0 },
            size: CGSize {
                width: 1.0,
                height: 1.0,
            },
        };
        let this = mtm.alloc::<Self>().set_ivars(());
        unsafe { msg_send_id![super(this), initWithFrame: frame] }
    }

    fn parent_view(&self) -> Option<Retained<WinitView>> {
        let sv = self.superview()?;
        if !sv.isKindOfClass(WinitView::class()) {
            return None;
        }
        // SAFETY: `isKindOfClass` verified `sv` is a `WinitView`.
        Some(unsafe { Retained::cast(sv) })
    }
}

declare_class!(
    pub(crate) struct WinitView;

    unsafe impl ClassType for WinitView {
        #[inherits(UIResponder, NSObject)]
        type Super = UIView;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "WinitUIView";
    }

    impl DeclaredClass for WinitView {
        type Ivars = WinitViewState;
    }

    unsafe impl WinitView {
        #[method(drawRect:)]
        fn draw_rect(&self, rect: CGRect) {
            let mtm = MainThreadMarker::new().unwrap();
            let window = self.window().unwrap();
            app_state::handle_nonuser_event(
                mtm,
                EventWrapper::StaticEvent(Event::WindowEvent {
                    window_id: RootWindowId(window.id()),
                    event: WindowEvent::RedrawRequested,
                }),
            );
            let _: () = unsafe { msg_send![super(self), drawRect: rect] };
        }

        #[method(layoutSubviews)]
        fn layout_subviews(&self) {
            let mtm = MainThreadMarker::new().unwrap();
            let _: () = unsafe { msg_send![super(self), layoutSubviews] };

            let window = self.window().unwrap();
            self.disable_hierarchy_gesture_delays();
            let window_bounds = window.bounds();
            // If the app is started in landscape, the view frame and window bounds can be mismatched.
            // The view frame will be in portrait and the window bounds in landscape. So apply the
            // window bounds to the view frame to make it consistent.
            let view_frame = self.frame();
            if view_frame != window_bounds {
                self.setFrame(window_bounds);
            }

            // Match Window::inner_size, the Metal layer, and view-local input.
            // Query after correcting the frame so rotation cannot report old bounds.
            let bounds = self.bounds();
            let size = crate::dpi::LogicalSize {
                width: bounds.size.width as f64,
                height: bounds.size.height as f64,
            }
            .to_physical(self.contentScaleFactor() as f64);

            app_state::handle_nonuser_event(
                mtm,
                EventWrapper::StaticEvent(Event::WindowEvent {
                    window_id: RootWindowId(window.id()),
                    event: WindowEvent::Resized(size),
                }),
            );
        }

        #[method(setContentScaleFactor:)]
        fn set_content_scale_factor(&self, untrusted_scale_factor: CGFloat) {
            let mtm = MainThreadMarker::new().unwrap();
            let _: () =
                unsafe { msg_send![super(self), setContentScaleFactor: untrusted_scale_factor] };

            // `window` is null when `setContentScaleFactor` is invoked prior to `[UIWindow
            // makeKeyAndVisible]` at window creation time (either manually or internally by
            // UIKit when the `UIView` is first created), in which case we send no events here
            let window = match self.window() {
                Some(window) => window,
                None => return,
            };
            // `setContentScaleFactor` may be called with a value of 0, which means "reset the
            // content scale factor to a device-specific default value", so we can't use the
            // parameter here. We can query the actual factor using the getter
            let scale_factor = self.contentScaleFactor();
            assert!(
                !scale_factor.is_nan()
                    && scale_factor.is_finite()
                    && scale_factor.is_sign_positive()
                    && scale_factor > 0.0,
                "invalid scale_factor set on UIView",
            );
            let scale_factor = scale_factor as f64;
            let bounds = self.bounds();
            let size = crate::dpi::LogicalSize {
                width: bounds.size.width as f64,
                height: bounds.size.height as f64,
            };
            let window_id = RootWindowId(window.id());
            app_state::handle_nonuser_events(
                mtm,
                std::iter::once(EventWrapper::ScaleFactorChanged(
                    app_state::ScaleFactorChanged {
                        window,
                        scale_factor,
                        suggested_size: size.to_physical(scale_factor),
                    },
                ))
                .chain(std::iter::once(EventWrapper::StaticEvent(
                    Event::WindowEvent {
                        window_id,
                        event: WindowEvent::Resized(size.to_physical(scale_factor)),
                    },
                ))),
            );
        }

        #[method(touchesBegan:withEvent:)]
        fn touches_began(&self, touches: &NSSet<UITouch>, event: Option<&UIEvent>) {
            self.handle_touches(touches, event)
        }

        #[method(touchesMoved:withEvent:)]
        fn touches_moved(&self, touches: &NSSet<UITouch>, event: Option<&UIEvent>) {
            self.handle_touches(touches, event)
        }

        #[method(touchesEnded:withEvent:)]
        fn touches_ended(&self, touches: &NSSet<UITouch>, event: Option<&UIEvent>) {
            self.handle_touches(touches, event)
        }

        #[method(touchesCancelled:withEvent:)]
        fn touches_cancelled(&self, touches: &NSSet<UITouch>, event: Option<&UIEvent>) {
            self.handle_touches(touches, event)
        }

        #[method(pinchGesture:)]
        fn pinch_gesture(&self, recognizer: &UIPinchGestureRecognizer) {
            let window = self.window().unwrap();

            let (phase, delta) = match recognizer.state() {
                UIGestureRecognizerState::Began => {
                    self.ivars().pinch_last_delta.set(recognizer.scale());
                    (TouchPhase::Started, 0.0)
                }
                UIGestureRecognizerState::Changed => {
                    let last_scale: f64 = self.ivars().pinch_last_delta.replace(recognizer.scale());
                    (TouchPhase::Moved, recognizer.scale() - last_scale)
                }
                UIGestureRecognizerState::Ended => {
                    let last_scale: f64 = self.ivars().pinch_last_delta.replace(0.0);
                    (TouchPhase::Moved, recognizer.scale() - last_scale)
                }
                UIGestureRecognizerState::Cancelled | UIGestureRecognizerState::Failed => {
                    self.ivars().rotation_last_delta.set(0.0);
                    // Pass -delta so that action is reversed
                    (TouchPhase::Cancelled, -recognizer.scale())
                }
                state => panic!("unexpected recognizer state: {state:?}"),
            };

            let gesture_event = EventWrapper::StaticEvent(Event::WindowEvent {
                window_id: RootWindowId(window.id()),
                event: WindowEvent::PinchGesture {
                    device_id: DEVICE_ID,
                    delta: delta as f64,
                    phase,
                },
            });

            let mtm = MainThreadMarker::new().unwrap();
            app_state::handle_nonuser_event(mtm, gesture_event);
        }

        #[method(doubleTapGesture:)]
        fn double_tap_gesture(&self, recognizer: &UITapGestureRecognizer) {
            let window = self.window().unwrap();

            if recognizer.state() == UIGestureRecognizerState::Ended {
                let gesture_event = EventWrapper::StaticEvent(Event::WindowEvent {
                    window_id: RootWindowId(window.id()),
                    event: WindowEvent::DoubleTapGesture {
                        device_id: DEVICE_ID,
                    },
                });

                let mtm = MainThreadMarker::new().unwrap();
                app_state::handle_nonuser_event(mtm, gesture_event);
            }
        }

        #[method(rotationGesture:)]
        fn rotation_gesture(&self, recognizer: &UIRotationGestureRecognizer) {
            let window = self.window().unwrap();

            let (phase, delta) = match recognizer.state() {
                UIGestureRecognizerState::Began => {
                    self.ivars().rotation_last_delta.set(0.0);

                    (TouchPhase::Started, 0.0)
                }
                UIGestureRecognizerState::Changed => {
                    let last_rotation = self.ivars().rotation_last_delta.replace(recognizer.rotation());

                    (TouchPhase::Moved, recognizer.rotation() - last_rotation)
                }
                UIGestureRecognizerState::Ended => {
                    let last_rotation = self.ivars().rotation_last_delta.replace(0.0);

                    (TouchPhase::Ended, recognizer.rotation() - last_rotation)
                }
                UIGestureRecognizerState::Cancelled | UIGestureRecognizerState::Failed => {
                    self.ivars().rotation_last_delta.set(0.0);

                    // Pass -delta so that action is reversed
                    (TouchPhase::Cancelled, -recognizer.rotation())
                }
                state => panic!("unexpected recognizer state: {state:?}"),
            };

            // Make delta negative to match macos, convert to degrees
            let gesture_event = EventWrapper::StaticEvent(Event::WindowEvent {
                window_id: RootWindowId(window.id()),
                event: WindowEvent::RotationGesture {
                    device_id: DEVICE_ID,
                    delta: -delta.to_degrees() as _,
                    phase,
                },
            });

            let mtm = MainThreadMarker::new().unwrap();
            app_state::handle_nonuser_event(mtm, gesture_event);
        }

        #[method(panGesture:)]
        fn pan_gesture(&self, recognizer: &UIPanGestureRecognizer) {
            let window = self.window().unwrap();

            let translation = recognizer.translationInView(Some(self));

            let (phase, dx, dy) = match recognizer.state() {
                UIGestureRecognizerState::Began => {
                    self.ivars().pan_last_delta.set(translation);

                    (TouchPhase::Started, 0.0, 0.0)
                }
                UIGestureRecognizerState::Changed => {
                    let last_pan: CGPoint = self.ivars().pan_last_delta.replace(translation);

                    let dx = translation.x - last_pan.x;
                    let dy = translation.y - last_pan.y;

                    (TouchPhase::Moved, dx, dy)
                }
                UIGestureRecognizerState::Ended => {
                    let last_pan: CGPoint = self.ivars().pan_last_delta.replace(CGPoint{x:0.0, y:0.0});

                    let dx = translation.x - last_pan.x;
                    let dy = translation.y - last_pan.y;

                    (TouchPhase::Ended, dx, dy)
                }
                UIGestureRecognizerState::Cancelled | UIGestureRecognizerState::Failed => {
                    let last_pan: CGPoint = self.ivars().pan_last_delta.replace(CGPoint{x:0.0, y:0.0});

                    // Pass -delta so that action is reversed
                    (TouchPhase::Cancelled, -last_pan.x, -last_pan.y)
                }
                state => panic!("unexpected recognizer state: {state:?}"),
            };


            let gesture_event = EventWrapper::StaticEvent(Event::WindowEvent {
                window_id: RootWindowId(window.id()),
                event: WindowEvent::PanGesture {
                    device_id: DEVICE_ID,
                    delta: PhysicalPosition::new(dx as _, dy as _),
                    phase,
                },
            });

            let mtm = MainThreadMarker::new().unwrap();
            app_state::handle_nonuser_event(mtm, gesture_event);
        }

        #[method(didMoveToWindow)]
        fn did_move_to_window(&self) {
            let _: () = unsafe { msg_send![super(self), didMoveToWindow] };
            self.ivars().window_owns_direct_touches.set(false);
            self.cancel_all_direct_touches();
            if (**self).window().is_some() {
                self.disable_hierarchy_gesture_delays();
                unsafe {
                    if self.ivars().ime_allowed.get() {
                        if self.ivars().ime_view.superview().is_none() {
                            self.addSubview(&self.ivars().ime_view);
                        }
                        self.ivars().ime_view.becomeFirstResponder();
                    } else {
                        self.becomeFirstResponder();
                    }
                }
            }
        }

        #[method(resignFirstResponder)]
        fn resign_first_responder(&self) -> bool {
            if !self.ivars().ime_allowed.get() {
                self.release_hardware_keys();
                self.set_mouse_captured(false);
            }
            unsafe { msg_send![super(self), resignFirstResponder] }
        }

        #[method(pressesBegan:withEvent:)]
        fn presses_began(&self, presses: &NSSet<UIPress>, event: Option<&UIPressesEvent>) {
            if !self.handle_presses(presses, ElementState::Pressed) {
                let _: () = unsafe { msg_send![super(self), pressesBegan: presses withEvent: event] };
            }
        }

        #[method(pressesEnded:withEvent:)]
        fn presses_ended(&self, presses: &NSSet<UIPress>, event: Option<&UIPressesEvent>) {
            if !self.handle_presses(presses, ElementState::Released) {
                let _: () = unsafe { msg_send![super(self), pressesEnded: presses withEvent: event] };
            }
        }

        #[method(pressesCancelled:withEvent:)]
        fn presses_cancelled(&self, presses: &NSSet<UIPress>, event: Option<&UIPressesEvent>) {
            if !self.handle_presses(presses, ElementState::Released) {
                let _: () = unsafe { msg_send![super(self), pressesCancelled: presses withEvent: event] };
            }
        }

        #[method(hardwareHover:)]
        fn hardware_hover(&self, recognizer: &UIHoverGestureRecognizer) {
            if self.ivars().mouse.captured() { return; }
            let Some(window) = self.window() else { return };
            let at = unsafe { recognizer.locationInView(Some(self)) };
            app_state::handle_nonuser_event(MainThreadMarker::from(self),
                EventWrapper::StaticEvent(Event::WindowEvent { window_id: RootWindowId(window.id()),
                    event: WindowEvent::CursorMoved { device_id: DEVICE_ID,
                        position: PhysicalPosition::new(at.x * self.contentScaleFactor(), at.y * self.contentScaleFactor()) } }));
        }

        #[method(canBecomeFirstResponder)]
        fn can_become_first_responder(&self) -> bool {
            true
        }

        // WinitView stays first responder during gameplay for hardware keyboard events.
        // Opt out of iOS's 3-finger text-editing gestures (Undo/Redo/Copy/Paste) so
        // multi-touch gameplay never gets hijacked and cancelled by UIKit.
        #[method(editingInteractionConfiguration)]
        fn editing_interaction_configuration(&self) -> UIEditingInteractionConfiguration {
            UIEditingInteractionConfiguration::None
        }
    }

    unsafe impl NSObjectProtocol for WinitView {}

    unsafe impl UIGestureRecognizerDelegate for WinitView {
        #[method(gestureRecognizer:shouldRecognizeSimultaneouslyWithGestureRecognizer:)]
        fn should_recognize_simultaneously(&self, _gesture_recognizer: &UIGestureRecognizer, _other_gesture_recognizer: &UIGestureRecognizer) -> bool {
            true
        }
    }
);

impl WinitView {
    pub(crate) fn new(
        mtm: MainThreadMarker,
        window_attributes: &WindowAttributes,
        frame: CGRect,
    ) -> Retained<Self> {
        let ime_view = WinitTextInputView::new(mtm);
        let this = mtm.alloc().set_ivars(WinitViewState {
            pinch_gesture_recognizer: RefCell::new(None),
            doubletap_gesture_recognizer: RefCell::new(None),
            rotation_gesture_recognizer: RefCell::new(None),
            pan_gesture_recognizer: RefCell::new(None),

            rotation_last_delta: Cell::new(0.0),
            pinch_last_delta: Cell::new(0.0),
            pan_last_delta: Cell::new(CGPoint { x: 0.0, y: 0.0 }),
            ime_allowed: Cell::new(false),
            ime_view,
            held_keys: RefCell::new(HashMap::new()),
            mouse: super::hardware_mouse::Mouse::default(),
            active_direct_touches: RefCell::new(HashMap::new()),
            window_owns_direct_touches: Cell::new(false),
        });
        let this: Retained<Self> = unsafe { msg_send_id![super(this), initWithFrame: frame] };

        this.setMultipleTouchEnabled(true);
        unsafe {
            this.setExclusiveTouch(false);
        }
        let hover = unsafe {
            UIHoverGestureRecognizer::initWithTarget_action(
                mtm.alloc(),
                Some(&*this),
                Some(sel!(hardwareHover:)),
            )
        };
        unsafe {
            hover.setCancelsTouchesInView(false);
            hover.setDelaysTouchesBegan(false);
            hover.setDelaysTouchesEnded(false);
        }
        this.addGestureRecognizer(&hover);

        if let Some(scale_factor) = window_attributes.platform_specific.scale_factor {
            this.setContentScaleFactor(scale_factor as _);
        }

        this
    }

    fn window(&self) -> Option<Retained<WinitUIWindow>> {
        // SAFETY: `WinitView`s are always installed in a `WinitUIWindow`
        (**self)
            .window()
            .map(|window| unsafe { Retained::cast(window) })
    }

    pub(crate) fn recognize_pinch_gesture(&self, should_recognize: bool) {
        let mtm = MainThreadMarker::from(self);
        if should_recognize {
            if self.ivars().pinch_gesture_recognizer.borrow().is_none() {
                let pinch = unsafe {
                    UIPinchGestureRecognizer::initWithTarget_action(
                        mtm.alloc(),
                        Some(self),
                        Some(sel!(pinchGesture:)),
                    )
                };
                pinch.setDelegate(Some(ProtocolObject::from_ref(self)));
                self.addGestureRecognizer(&pinch);
                self.ivars().pinch_gesture_recognizer.replace(Some(pinch));
            }
        } else if let Some(recognizer) = self.ivars().pinch_gesture_recognizer.take() {
            self.removeGestureRecognizer(&recognizer);
        }
    }

    pub(crate) fn recognize_pan_gesture(
        &self,
        should_recognize: bool,
        minimum_number_of_touches: u8,
        maximum_number_of_touches: u8,
    ) {
        let mtm = MainThreadMarker::from(self);
        if should_recognize {
            if self.ivars().pan_gesture_recognizer.borrow().is_none() {
                let pan = unsafe {
                    UIPanGestureRecognizer::initWithTarget_action(
                        mtm.alloc(),
                        Some(self),
                        Some(sel!(panGesture:)),
                    )
                };
                pan.setDelegate(Some(ProtocolObject::from_ref(self)));
                pan.setMinimumNumberOfTouches(minimum_number_of_touches as _);
                pan.setMaximumNumberOfTouches(maximum_number_of_touches as _);
                self.addGestureRecognizer(&pan);
                self.ivars().pan_gesture_recognizer.replace(Some(pan));
            }
        } else if let Some(recognizer) = self.ivars().pan_gesture_recognizer.take() {
            self.removeGestureRecognizer(&recognizer);
        }
    }

    pub(crate) fn recognize_doubletap_gesture(&self, should_recognize: bool) {
        let mtm = MainThreadMarker::from(self);
        if should_recognize {
            if self.ivars().doubletap_gesture_recognizer.borrow().is_none() {
                let tap = unsafe {
                    UITapGestureRecognizer::initWithTarget_action(
                        mtm.alloc(),
                        Some(self),
                        Some(sel!(doubleTapGesture:)),
                    )
                };
                tap.setDelegate(Some(ProtocolObject::from_ref(self)));
                tap.setNumberOfTapsRequired(2);
                tap.setNumberOfTouchesRequired(1);
                self.addGestureRecognizer(&tap);
                self.ivars().doubletap_gesture_recognizer.replace(Some(tap));
            }
        } else if let Some(recognizer) = self.ivars().doubletap_gesture_recognizer.take() {
            self.removeGestureRecognizer(&recognizer);
        }
    }

    pub(crate) fn recognize_rotation_gesture(&self, should_recognize: bool) {
        let mtm = MainThreadMarker::from(self);
        if should_recognize {
            if self.ivars().rotation_gesture_recognizer.borrow().is_none() {
                let rotation = unsafe {
                    UIRotationGestureRecognizer::initWithTarget_action(
                        mtm.alloc(),
                        Some(self),
                        Some(sel!(rotationGesture:)),
                    )
                };
                rotation.setDelegate(Some(ProtocolObject::from_ref(self)));
                self.addGestureRecognizer(&rotation);
                self.ivars()
                    .rotation_gesture_recognizer
                    .replace(Some(rotation));
            }
        } else if let Some(recognizer) = self.ivars().rotation_gesture_recognizer.take() {
            self.removeGestureRecognizer(&recognizer);
        }
    }

    fn disable_hierarchy_gesture_delays(&self) {
        let mut current: Option<Retained<UIView>> = self.superview();
        while let Some(view) = current {
            disable_recognizer_delays(&view);
            current = view.superview();
        }
        if let Some(window) = (**self).window() {
            disable_system_gesture_delays(&window);
        }
    }

    pub(crate) fn set_window_owns_direct_touches(&self, owned: bool) {
        self.ivars().window_owns_direct_touches.set(owned);
    }

    pub(crate) fn cancel_all_direct_touches(&self) {
        let cancelled: Vec<(u64, PhysicalPosition<f64>)> =
            self.ivars().active_direct_touches.borrow_mut().drain().collect();
        if cancelled.is_empty() {
            return;
        }
        let Some(window) = self.window() else {
            return;
        };
        let window_id = RootWindowId(window.id());
        let events: Vec<EventWrapper> = cancelled
            .into_iter()
            .map(|(id, location)| {
                EventWrapper::StaticEvent(Event::WindowEvent {
                    window_id,
                    event: WindowEvent::Touch(Touch {
                        device_id: DEVICE_ID,
                        id,
                        location,
                        force: None,
                        phase: TouchPhase::Cancelled,
                    }),
                })
            })
            .collect();
        let mtm = MainThreadMarker::new().unwrap();
        app_state::handle_nonuser_events(mtm, events);
    }

    fn touch_force(&self, touch: &UITouch, touch_type: UITouchType, os_supports_force: bool) -> Option<Force> {
        if !os_supports_force {
            return None;
        }
        let trait_collection = self.traitCollection();
        let touch_capability = trait_collection.forceTouchCapability();
        // Both the OS _and_ the device need to be checked for force touch support.
        if touch_capability == UIForceTouchCapability::Available
            || touch_type == UITouchType::Pencil
        {
            let force = touch.force();
            let max_possible_force = touch.maximumPossibleForce();
            let altitude_angle: Option<f64> = if touch_type == UITouchType::Pencil {
                let angle = touch.altitudeAngle();
                Some(angle as _)
            } else {
                None
            };
            Some(Force::Calibrated {
                force: force as _,
                max_possible_force: max_possible_force as _,
                altitude_angle,
            })
        } else {
            None
        }
    }

    fn touch_physical_location(&self, touch: &UITouch) -> PhysicalPosition<f64> {
        // Hover, direct touches and trackpad clicks share the render-view origin.
        let logical_location = touch.locationInView(Some(self));
        let scale_factor = self.contentScaleFactor();
        PhysicalPosition::from_logical::<(f64, f64), f64>(
            (logical_location.x as _, logical_location.y as _),
            scale_factor as f64,
        )
    }

    fn touch_event(
        window_id: RootWindowId,
        id: u64,
        location: PhysicalPosition<f64>,
        force: Option<Force>,
        phase: TouchPhase,
    ) -> EventWrapper {
        EventWrapper::StaticEvent(Event::WindowEvent {
            window_id,
            event: WindowEvent::Touch(Touch {
                device_id: DEVICE_ID,
                id,
                location,
                force,
                phase,
            }),
        })
    }

    /// Snapshot direct touches from `UIEvent` *before* `[UIWindow sendEvent:]` mutates or clears
    /// `UITouchPhase::Ended` / `Cancelled` state, while still respecting hit-testing so modal
    /// sheets, pickers, alerts, other windows, and IME views keep their own touches.
    /// The caller (`WinitUIWindow::sendEvent:`) dispatches the returned events *after* `[super
    /// sendEvent:]` returns to avoid UIKit re-entrancy hazards.
    pub(crate) fn collect_window_touch_events(
        &self,
        window: &WinitUIWindow,
        event: &UIEvent,
    ) -> Vec<EventWrapper> {
        if unsafe { event.r#type() } != UIEventType::Touches {
            return Vec::new();
        }
        let Some(all_touches) = (unsafe { event.allTouches() }) else {
            return Vec::new();
        };
        let window_id = RootWindowId(window.id());
        let window_ui: &UIWindow = window;
        let window_view: &UIView = window_ui;
        let self_view: &UIView = self;
        let os_supports_force = app_state::os_capabilities().force_touch;
        let mut touch_events = Vec::new();

        {
            let mut active = self.ivars().active_direct_touches.borrow_mut();
            for touch in all_touches.iter() {
                let touch_type = touch.r#type();
                if touch_type == UITouchType::IndirectPointer {
                    continue;
                }
                let touch_id = touch as *const UITouch as u64;
                let phase = touch.phase();
                let already_active = active.contains_key(&touch_id);
                let in_this_window = touch
                    .window()
                    .map_or(true, |w| std::ptr::eq(&*w, window_ui));

                match phase {
                    UITouchPhase::Began => {
                        if !in_this_window {
                            continue;
                        }
                        let hits_self = match touch.view() {
                            Some(v) => std::ptr::eq(&*v, self_view),
                            None => {
                                let pt = touch.locationInView(Some(window_view));
                                unsafe { window_view.hitTest_withEvent(pt, Some(event)) }
                                    .is_some_and(|hit| std::ptr::eq(&*hit, self_view))
                            }
                        };
                        if !hits_self {
                            continue;
                        }
                        let physical_location = self.touch_physical_location(touch);
                        let force = self.touch_force(touch, touch_type, os_supports_force);
                        if let Some(stale_location) = active.insert(touch_id, physical_location) {
                            touch_events.push(Self::touch_event(
                                window_id,
                                touch_id,
                                stale_location,
                                None,
                                TouchPhase::Cancelled,
                            ));
                        }
                        touch_events.push(Self::touch_event(
                            window_id,
                            touch_id,
                            physical_location,
                            force,
                            TouchPhase::Started,
                        ));
                    }
                    UITouchPhase::Moved => {
                        if !already_active
                            && !(in_this_window
                                && touch.view().is_some_and(|v| std::ptr::eq(&*v, self_view)))
                        {
                            continue;
                        }
                        let physical_location = self.touch_physical_location(touch);
                        let force = self.touch_force(touch, touch_type, os_supports_force);
                        let out_phase = if active.insert(touch_id, physical_location).is_none() {
                            TouchPhase::Started
                        } else {
                            TouchPhase::Moved
                        };
                        touch_events.push(Self::touch_event(
                            window_id,
                            touch_id,
                            physical_location,
                            force,
                            out_phase,
                        ));
                    }
                    UITouchPhase::Stationary => {
                        if !already_active
                            && in_this_window
                            && touch.view().is_some_and(|v| std::ptr::eq(&*v, self_view))
                        {
                            let physical_location = self.touch_physical_location(touch);
                            let force = self.touch_force(touch, touch_type, os_supports_force);
                            active.insert(touch_id, physical_location);
                            touch_events.push(Self::touch_event(
                                window_id,
                                touch_id,
                                physical_location,
                                force,
                                TouchPhase::Started,
                            ));
                        }
                    }
                    UITouchPhase::Ended => {
                        if active.remove(&touch_id).is_some() {
                            let physical_location = self.touch_physical_location(touch);
                            let force = self.touch_force(touch, touch_type, os_supports_force);
                            touch_events.push(Self::touch_event(
                                window_id,
                                touch_id,
                                physical_location,
                                force,
                                TouchPhase::Ended,
                            ));
                        }
                    }
                    UITouchPhase::Cancelled => {
                        if active.remove(&touch_id).is_some() {
                            let physical_location = self.touch_physical_location(touch);
                            let force = self.touch_force(touch, touch_type, os_supports_force);
                            touch_events.push(Self::touch_event(
                                window_id,
                                touch_id,
                                physical_location,
                                force,
                                TouchPhase::Cancelled,
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }

        touch_events
    }

    fn handle_touches(&self, touches: &NSSet<UITouch>, event: Option<&UIEvent>) {
        let Some(window) = self.window() else {
            return;
        };
        let window_id = RootWindowId(window.id());
        let window_owns_direct = self.ivars().window_owns_direct_touches.get();
        let mut touch_events = Vec::new();
        let mut handled_ids = Vec::new();
        let os_supports_force = app_state::os_capabilities().force_touch;

        {
            let mut active = self.ivars().active_direct_touches.borrow_mut();

            for touch in touches {
                let touch_type = touch.r#type();
                if touch_type == UITouchType::IndirectPointer {
                    if self.ivars().mouse.captured() {
                        continue;
                    }
                    let phase = match touch.phase() {
                        UITouchPhase::Began => TouchPhase::Started,
                        UITouchPhase::Moved => TouchPhase::Moved,
                        UITouchPhase::Ended => TouchPhase::Ended,
                        UITouchPhase::Cancelled => TouchPhase::Cancelled,
                        _ => continue,
                    };
                    let physical_location = self.touch_physical_location(touch);
                    touch_events.push(EventWrapper::StaticEvent(Event::WindowEvent {
                        window_id,
                        event: WindowEvent::CursorMoved {
                            device_id: DEVICE_ID,
                            position: physical_location,
                        },
                    }));
                    if matches!(
                        phase,
                        TouchPhase::Started | TouchPhase::Ended | TouchPhase::Cancelled
                    ) {
                        touch_events.push(EventWrapper::StaticEvent(Event::WindowEvent {
                            window_id,
                            event: WindowEvent::MouseInput {
                                device_id: DEVICE_ID,
                                button: crate::event::MouseButton::Left,
                                state: if phase == TouchPhase::Started {
                                    ElementState::Pressed
                                } else {
                                    ElementState::Released
                                },
                            },
                        }));
                    }
                    continue;
                }
                if window_owns_direct {
                    // `WinitUIWindow::sendEvent:` owns direct touches for this window.
                    continue;
                }
                let touch_id = touch as *const UITouch as u64;
                handled_ids.push(touch_id);
                let physical_location = self.touch_physical_location(touch);
                let force = self.touch_force(touch, touch_type, os_supports_force);
                match touch.phase() {
                    UITouchPhase::Began => {
                        if let Some(stale_location) = active.insert(touch_id, physical_location) {
                            touch_events.push(Self::touch_event(
                                window_id,
                                touch_id,
                                stale_location,
                                None,
                                TouchPhase::Cancelled,
                            ));
                        }
                        touch_events.push(Self::touch_event(
                            window_id,
                            touch_id,
                            physical_location,
                            force,
                            TouchPhase::Started,
                        ));
                    }
                    UITouchPhase::Moved => {
                        let out_phase = if active.insert(touch_id, physical_location).is_none() {
                            TouchPhase::Started
                        } else {
                            TouchPhase::Moved
                        };
                        touch_events.push(Self::touch_event(
                            window_id,
                            touch_id,
                            physical_location,
                            force,
                            out_phase,
                        ));
                    }
                    UITouchPhase::Ended => {
                        if active.remove(&touch_id).is_some() {
                            touch_events.push(Self::touch_event(
                                window_id,
                                touch_id,
                                physical_location,
                                force,
                                TouchPhase::Ended,
                            ));
                        }
                    }
                    UITouchPhase::Cancelled => {
                        if active.remove(&touch_id).is_some() {
                            touch_events.push(Self::touch_event(
                                window_id,
                                touch_id,
                                physical_location,
                                force,
                                TouchPhase::Cancelled,
                            ));
                        }
                    }
                    _ => {}
                }
            }

            // Foreign-window fallback (e.g. a host container reparenting `WinitView` into its own
            // `UIWindow`): also inspect `event.touchesForView(self)` for any direct touch held
            // back from `touchesBegan:` / `touchesMoved:` / `touchesEnded:` by a recognizer.
            if !window_owns_direct {
                if let Some(event) = event {
                    if let Some(view_touches) = unsafe { event.touchesForView(self) } {
                        for touch in view_touches.iter() {
                            let touch_type = touch.r#type();
                            if touch_type == UITouchType::IndirectPointer {
                                continue;
                            }
                            let touch_id = touch as *const UITouch as u64;
                            if handled_ids.contains(&touch_id) {
                                continue;
                            }
                            match touch.phase() {
                                UITouchPhase::Began => {
                                    let physical_location = self.touch_physical_location(touch);
                                    let force =
                                        self.touch_force(touch, touch_type, os_supports_force);
                                    if let Some(stale_location) =
                                        active.insert(touch_id, physical_location)
                                    {
                                        touch_events.push(Self::touch_event(
                                            window_id,
                                            touch_id,
                                            stale_location,
                                            None,
                                            TouchPhase::Cancelled,
                                        ));
                                    }
                                    touch_events.push(Self::touch_event(
                                        window_id,
                                        touch_id,
                                        physical_location,
                                        force,
                                        TouchPhase::Started,
                                    ));
                                }
                                UITouchPhase::Moved => {
                                    let physical_location = self.touch_physical_location(touch);
                                    let force =
                                        self.touch_force(touch, touch_type, os_supports_force);
                                    let out_phase =
                                        if active.insert(touch_id, physical_location).is_none() {
                                            TouchPhase::Started
                                        } else {
                                            TouchPhase::Moved
                                        };
                                    touch_events.push(Self::touch_event(
                                        window_id,
                                        touch_id,
                                        physical_location,
                                        force,
                                        out_phase,
                                    ));
                                }
                                UITouchPhase::Stationary => {
                                    if !active.contains_key(&touch_id) {
                                        let physical_location = self.touch_physical_location(touch);
                                        let force =
                                            self.touch_force(touch, touch_type, os_supports_force);
                                        active.insert(touch_id, physical_location);
                                        touch_events.push(Self::touch_event(
                                            window_id,
                                            touch_id,
                                            physical_location,
                                            force,
                                            TouchPhase::Started,
                                        ));
                                    }
                                }
                                UITouchPhase::Ended => {
                                    if active.remove(&touch_id).is_some() {
                                        let physical_location = self.touch_physical_location(touch);
                                        let force =
                                            self.touch_force(touch, touch_type, os_supports_force);
                                        touch_events.push(Self::touch_event(
                                            window_id,
                                            touch_id,
                                            physical_location,
                                            force,
                                            TouchPhase::Ended,
                                        ));
                                    }
                                }
                                UITouchPhase::Cancelled => {
                                    if active.remove(&touch_id).is_some() {
                                        let physical_location = self.touch_physical_location(touch);
                                        let force =
                                            self.touch_force(touch, touch_type, os_supports_force);
                                        touch_events.push(Self::touch_event(
                                            window_id,
                                            touch_id,
                                            physical_location,
                                            force,
                                            TouchPhase::Cancelled,
                                        ));
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }

        if !touch_events.is_empty() {
            let mtm = MainThreadMarker::new().unwrap();
            app_state::handle_nonuser_events(mtm, touch_events);
        }
    }

    pub(crate) fn set_mouse_captured(&self, captured: bool) {
        self.ivars().mouse.capture(captured);
        self.update_mice();
    }

    pub(crate) fn update_mice(&self) {
        if let Some(window) = self.window() {
            self.ivars().mouse.update(RootWindowId(window.id()));
        }
    }

    pub(crate) fn set_ime_allowed(&self, allowed: bool) {
        self.ivars().ime_allowed.set(allowed);
        unsafe {
            if allowed {
                if self.ivars().ime_view.superview().is_none() {
                    self.addSubview(&self.ivars().ime_view);
                }
                self.ivars().ime_view.becomeFirstResponder();
            } else {
                if self.ivars().ime_view.isFirstResponder() {
                    self.ivars().ime_view.resignFirstResponder();
                }
                if self.ivars().ime_view.superview().is_some() {
                    self.ivars().ime_view.removeFromSuperview();
                }
                if self.window().is_some() {
                    self.becomeFirstResponder();
                }
            }
        }
    }

    fn emit_hardware_key(&self, event: KeyEvent) {
        let Some(window) = self.window() else { return };
        app_state::handle_nonuser_event(
            MainThreadMarker::from(self),
            EventWrapper::StaticEvent(Event::WindowEvent {
                window_id: RootWindowId(window.id()),
                event: WindowEvent::KeyboardInput {
                    device_id: DEVICE_ID,
                    event,
                    is_synthetic: false,
                },
            }),
        );
    }

    fn release_hardware_keys(&self) {
        let held = std::mem::take(&mut *self.ivars().held_keys.borrow_mut());
        for (_, mut event) in held {
            event.state = ElementState::Released;
            event.text = None;
            event.repeat = false;
            self.emit_hardware_key(event);
        }
        if let Some(window) = self.window() {
            app_state::handle_nonuser_event(
                MainThreadMarker::from(self),
                EventWrapper::StaticEvent(Event::WindowEvent {
                    window_id: RootWindowId(window.id()),
                    event: WindowEvent::ModifiersChanged(ModifiersState::empty().into()),
                }),
            );
        }
    }

    fn handle_presses(&self, presses: &NSSet<UIPress>, state: ElementState) -> bool {
        let mut handled = false;
        for press in presses {
            // UIKey is present only for keyboard presses (iOS 13.4+, app minimum is 16).
            let Some(key) = (unsafe { press.key(MainThreadMarker::from(self)) }) else {
                continue;
            };
            handled = true;
            let usage = unsafe { key.keyCode() }.0 as usize;
            let physical_key = super::hardware_keyboard::physical_key(usage);
            let flags = unsafe { key.modifierFlags() }.0;
            let mut modifiers = ModifiersState::empty();
            modifiers.set(ModifiersState::SHIFT, flags & (1 << 17) != 0);
            modifiers.set(ModifiersState::CONTROL, flags & (1 << 18) != 0);
            modifiers.set(ModifiersState::ALT, flags & (1 << 19) != 0);
            modifiers.set(ModifiersState::SUPER, flags & (1 << 20) != 0);
            if let Some(window) = self.window() {
                app_state::handle_nonuser_event(
                    MainThreadMarker::from(self),
                    EventWrapper::StaticEvent(Event::WindowEvent {
                        window_id: RootWindowId(window.id()),
                        event: WindowEvent::ModifiersChanged(modifiers.into()),
                    }),
                );
            }
            let text = unsafe { key.characters() }.to_string();
            let logical_key = super::hardware_keyboard::logical_key(physical_key, &text);
            let event_text = if state == ElementState::Pressed {
                super::hardware_keyboard::key_text(&logical_key, &text)
            } else {
                None
            };
            let event = KeyEvent {
                physical_key,
                logical_key,
                text: event_text,
                location: super::hardware_keyboard::location(physical_key),
                state,
                repeat: state == ElementState::Pressed
                    && self.ivars().held_keys.borrow().contains_key(&usage),
                platform_specific: KeyEventExtra {},
            };
            if state == ElementState::Pressed {
                self.ivars()
                    .held_keys
                    .borrow_mut()
                    .insert(usage, event.clone());
            } else {
                self.ivars().held_keys.borrow_mut().remove(&usage);
            }
            self.emit_hardware_key(event);
        }
        handled
    }

    fn handle_insert_text(&self, text: &NSString) {
        let window = self.window().unwrap();
        let window_id = RootWindowId(window.id());
        let mtm = MainThreadMarker::new().unwrap();
        // Both software typing and hardware navigation can arrive via insertText:.
        // Decode navigation tokens before expanding genuine text into characters.
        app_state::handle_nonuser_events(
            mtm,
            super::hardware_keyboard::insert_text_keys(&text.to_string())
                .into_iter()
                .flat_map(|(physical_key, logical_key)| {
                    let text = match &logical_key {
                        Key::Character(text) => Some(text.clone()),
                        _ => None,
                    };
                    [ElementState::Pressed, ElementState::Released].map(|state| {
                        EventWrapper::StaticEvent(Event::WindowEvent {
                            window_id,
                            event: WindowEvent::KeyboardInput {
                                event: KeyEvent {
                                    text: if state == ElementState::Pressed {
                                        text.clone()
                                    } else {
                                        None
                                    },
                                    state,
                                    location: KeyLocation::Standard,
                                    repeat: false,
                                    logical_key: logical_key.clone(),
                                    physical_key,
                                    platform_specific: KeyEventExtra {},
                                },
                                is_synthetic: false,
                                device_id: DEVICE_ID,
                            },
                        })
                    })
                }),
        );
    }

    fn handle_delete_backward(&self) {
        let window = self.window().unwrap();
        let window_id = RootWindowId(window.id());
        let mtm = MainThreadMarker::new().unwrap();
        app_state::handle_nonuser_events(
            mtm,
            [ElementState::Pressed, ElementState::Released].map(|state| {
                EventWrapper::StaticEvent(Event::WindowEvent {
                    window_id,
                    event: WindowEvent::KeyboardInput {
                        device_id: DEVICE_ID,
                        event: KeyEvent {
                            state,
                            logical_key: Key::Named(NamedKey::Backspace),
                            physical_key: PhysicalKey::Code(KeyCode::Backspace),
                            platform_specific: KeyEventExtra {},
                            repeat: false,
                            location: KeyLocation::Standard,
                            text: None,
                        },
                        is_synthetic: false,
                    },
                })
            }),
        );
    }
}
