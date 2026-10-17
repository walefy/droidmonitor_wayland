use crate::cosmic_config::TOUCHSCREEN_DEVICE_NAME;
use anyhow::{Context, Result};
use evdev::{
    uinput::{VirtualDevice, VirtualDeviceBuilder},
    AbsInfo, AbsoluteAxisType, AttributeSet, EventType, InputEvent, Key, PropType, UinputAbsSetup,
};

pub const MAX_SLOTS: usize = 10;
pub const ABS_MAX: i32 = 32767;

#[derive(Debug, Clone, Copy)]
pub enum TouchAction {
    Down = 0,
    Move = 1,
    Up = 2,
    Cancel = 3,
}

impl TouchAction {
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            0 => Some(Self::Down),
            1 => Some(Self::Move),
            2 => Some(Self::Up),
            3 => Some(Self::Cancel),
            _ => None,
        }
    }
}

pub struct VirtualTouchscreen {
    device: VirtualDevice,
    active_slots: [bool; MAX_SLOTS],
    next_tracking_id: i32,
}

impl VirtualTouchscreen {
    pub fn new() -> Result<Self> {
        let mut keys = AttributeSet::<Key>::new();
        keys.insert(Key::BTN_TOUCH);

        let mut props = AttributeSet::<PropType>::new();
        props.insert(PropType::DIRECT);

        let abs_coord = AbsInfo::new(0, 0, ABS_MAX, 0, 0, 100);
        let abs_slot = AbsInfo::new(0, 0, (MAX_SLOTS - 1) as i32, 0, 0, 0);
        let abs_track = AbsInfo::new(0, 0, 65535, 0, 0, 0);

        let device = VirtualDeviceBuilder::new()
            .context("Falha ao abrir /dev/uinput (verifique permissões de uinput)")?
            .name(TOUCHSCREEN_DEVICE_NAME)
            .with_properties(&props)?
            .with_keys(&keys)?
            .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisType::ABS_X, abs_coord))?
            .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisType::ABS_Y, abs_coord))?
            .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisType::ABS_MT_SLOT, abs_slot))?
            .with_absolute_axis(&UinputAbsSetup::new(
                AbsoluteAxisType::ABS_MT_TRACKING_ID,
                abs_track,
            ))?
            .with_absolute_axis(&UinputAbsSetup::new(
                AbsoluteAxisType::ABS_MT_POSITION_X,
                abs_coord,
            ))?
            .with_absolute_axis(&UinputAbsSetup::new(
                AbsoluteAxisType::ABS_MT_POSITION_Y,
                abs_coord,
            ))?
            .build()
            .context("Falha ao registrar dispositivo uinput VirtualTouchscreen")?;

        Ok(Self {
            device,
            active_slots: [false; MAX_SLOTS],
            next_tracking_id: 1,
        })
    }

    /// Processes a binary touch packet: `[action: u8, slot: u8, x: u16 (LE), y: u16 (LE)]`
    /// where `x` and `y` are normalized `0..=32767`.
    pub fn handle_touch_event(&mut self, action: TouchAction, slot: u8, x: u16, y: u16) -> Result<()> {
        let slot_idx = (slot as usize).min(MAX_SLOTS - 1);
        let x = (x as i32).clamp(0, ABS_MAX);
        let y = (y as i32).clamp(0, ABS_MAX);

        let was_any_active = self.active_slots.iter().any(|&a| a);
        let mut events = Vec::with_capacity(8);

        events.push(InputEvent::new(
            EventType::ABSOLUTE,
            AbsoluteAxisType::ABS_MT_SLOT.0,
            slot_idx as i32,
        ));

        match action {
            TouchAction::Down => {
                if !self.active_slots[slot_idx] {
                    self.active_slots[slot_idx] = true;
                    let tracking_id = self.next_tracking_id;
                    self.next_tracking_id = (self.next_tracking_id % 65000) + 1;
                    events.push(InputEvent::new(
                        EventType::ABSOLUTE,
                        AbsoluteAxisType::ABS_MT_TRACKING_ID.0,
                        tracking_id,
                    ));
                }
                events.push(InputEvent::new(
                    EventType::ABSOLUTE,
                    AbsoluteAxisType::ABS_MT_POSITION_X.0,
                    x,
                ));
                events.push(InputEvent::new(
                    EventType::ABSOLUTE,
                    AbsoluteAxisType::ABS_MT_POSITION_Y.0,
                    y,
                ));
                if !was_any_active {
                    events.push(InputEvent::new(EventType::KEY, Key::BTN_TOUCH.0, 1));
                }
                if slot_idx == 0 {
                    events.push(InputEvent::new(
                        EventType::ABSOLUTE,
                        AbsoluteAxisType::ABS_X.0,
                        x,
                    ));
                    events.push(InputEvent::new(
                        EventType::ABSOLUTE,
                        AbsoluteAxisType::ABS_Y.0,
                        y,
                    ));
                }
            }
            TouchAction::Move => {
                if !self.active_slots[slot_idx] {
                    self.active_slots[slot_idx] = true;
                    let tracking_id = self.next_tracking_id;
                    self.next_tracking_id = (self.next_tracking_id % 65000) + 1;
                    events.push(InputEvent::new(
                        EventType::ABSOLUTE,
                        AbsoluteAxisType::ABS_MT_TRACKING_ID.0,
                        tracking_id,
                    ));
                    if !was_any_active {
                        events.push(InputEvent::new(EventType::KEY, Key::BTN_TOUCH.0, 1));
                    }
                }
                events.push(InputEvent::new(
                    EventType::ABSOLUTE,
                    AbsoluteAxisType::ABS_MT_POSITION_X.0,
                    x,
                ));
                events.push(InputEvent::new(
                    EventType::ABSOLUTE,
                    AbsoluteAxisType::ABS_MT_POSITION_Y.0,
                    y,
                ));
                if slot_idx == 0 {
                    events.push(InputEvent::new(
                        EventType::ABSOLUTE,
                        AbsoluteAxisType::ABS_X.0,
                        x,
                    ));
                    events.push(InputEvent::new(
                        EventType::ABSOLUTE,
                        AbsoluteAxisType::ABS_Y.0,
                        y,
                    ));
                }
            }
            TouchAction::Up | TouchAction::Cancel => {
                if self.active_slots[slot_idx] {
                    self.active_slots[slot_idx] = false;
                    events.push(InputEvent::new(
                        EventType::ABSOLUTE,
                        AbsoluteAxisType::ABS_MT_TRACKING_ID.0,
                        -1,
                    ));
                }
                let is_any_active = self.active_slots.iter().any(|&a| a);
                if was_any_active && !is_any_active {
                    events.push(InputEvent::new(EventType::KEY, Key::BTN_TOUCH.0, 0));
                }
            }
        }

        self.device.emit(&events)?;
        Ok(())
    }
}
