use std::time::Duration;

use crate::message::types::TopologyFlag;

pub const IDLE_SLEEP_TIMEOUT: Duration = Duration::from_secs(5 * 60);
pub const SYNCHRONIZED_WAKE_INTERVAL: Duration = Duration::from_secs(5);
pub const SYNCHRONIZED_WAKE_WINDOW: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerMode {
    HighPower,
    SynchronizedLowPower,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterfacePowerState {
    pub ble_enabled: bool,
    pub wifi_enabled: bool,
}

impl InterfacePowerState {
    const fn awake() -> Self {
        Self {
            ble_enabled: true,
            wifi_enabled: true,
        }
    }

    const fn sleeping() -> Self {
        Self {
            ble_enabled: false,
            wifi_enabled: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowerDecision {
    pub interface_state: InterfacePowerState,
    pub topology_flags: Vec<TopologyFlag>,
    pub wake_network: bool,
}

pub struct PowerManager {
    mode: PowerMode,
    last_incoming_activity_at: Duration,
    pending_outbound_transaction: bool,
    sleep_flag_emitted: bool,
}

impl PowerManager {
    pub fn new(now: Duration) -> Self {
        Self {
            mode: PowerMode::HighPower,
            last_incoming_activity_at: now,
            pending_outbound_transaction: false,
            sleep_flag_emitted: false,
        }
    }

    pub fn mode(&self) -> PowerMode {
        self.mode
    }

    pub fn interface_state(&self, now: Duration) -> InterfacePowerState {
        if self.mode == PowerMode::HighPower || self.is_awake_window(now) {
            InterfacePowerState::awake()
        } else {
            InterfacePowerState::sleeping()
        }
    }

    pub fn is_awake_window(&self, now: Duration) -> bool {
        let interval_ms = SYNCHRONIZED_WAKE_INTERVAL.as_millis();
        let window_ms = SYNCHRONIZED_WAKE_WINDOW.as_millis();
        now.as_millis() % interval_ms < window_ms
    }

    pub fn next_awake_barrier(&self, now: Duration) -> Duration {
        let interval_ms = SYNCHRONIZED_WAKE_INTERVAL.as_millis();
        let current_ms = now.as_millis();
        let next_ms = ((current_ms / interval_ms) + 1) * interval_ms;
        Duration::from_millis(next_ms as u64)
    }

    pub fn record_incoming_transaction(&mut self, now: Duration) -> PowerDecision {
        self.last_incoming_activity_at = now;
        self.pending_outbound_transaction = false;
        self.sleep_flag_emitted = false;
        self.mode = PowerMode::HighPower;
        self.tick(now)
    }

    pub fn record_outbound_transaction(&mut self, now: Duration) -> PowerDecision {
        let mut decision = self.tick(now);

        if self.mode == PowerMode::SynchronizedLowPower && !self.is_awake_window(now) {
            self.pending_outbound_transaction = true;
            decision.interface_state = InterfacePowerState::sleeping();
            return decision;
        }

        self.last_incoming_activity_at = now;
        self.sleep_flag_emitted = false;
        self.mode = PowerMode::HighPower;
        decision.interface_state = InterfacePowerState::awake();
        decision
    }

    pub fn tick(&mut self, now: Duration) -> PowerDecision {
        let mut topology_flags = Vec::new();
        let idle_for = now.saturating_sub(self.last_incoming_activity_at);

        if idle_for >= IDLE_SLEEP_TIMEOUT && self.mode == PowerMode::HighPower {
            self.mode = PowerMode::SynchronizedLowPower;
        }

        if self.mode == PowerMode::SynchronizedLowPower && !self.sleep_flag_emitted {
            topology_flags.push(TopologyFlag::GoToSleep);
            self.sleep_flag_emitted = true;
        }

        let wake_network = self.mode == PowerMode::SynchronizedLowPower
            && self.pending_outbound_transaction
            && self.is_awake_window(now);

        if wake_network {
            self.mode = PowerMode::HighPower;
            self.pending_outbound_transaction = false;
            self.last_incoming_activity_at = now;
            self.sleep_flag_emitted = false;
        }

        PowerDecision {
            interface_state: self.interface_state(now),
            topology_flags,
            wake_network,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn awake_window_alignment_uses_shared_modulo_barrier() {
        let manager = PowerManager::new(Duration::from_secs(0));
        assert!(manager.is_awake_window(Duration::from_millis(10)));
        assert!(!manager.is_awake_window(Duration::from_millis(4_999)));
        assert!(manager.is_awake_window(Duration::from_millis(5_000)));
        assert!(manager.is_awake_window(Duration::from_millis(5_499)));
        assert!(!manager.is_awake_window(Duration::from_millis(5_500)));
    }

    #[test]
    fn next_awake_barrier_rounds_up_to_next_five_second_boundary() {
        let manager = PowerManager::new(Duration::from_secs(0));
        assert_eq!(
            manager.next_awake_barrier(Duration::from_millis(1_250)),
            Duration::from_secs(5)
        );
        assert_eq!(
            manager.next_awake_barrier(Duration::from_secs(5)),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn idle_timeout_emits_single_go_to_sleep_flag() {
        let mut manager = PowerManager::new(Duration::from_secs(0));

        let first = manager.tick(IDLE_SLEEP_TIMEOUT);
        assert_eq!(manager.mode(), PowerMode::SynchronizedLowPower);
        assert_eq!(first.topology_flags, vec![TopologyFlag::GoToSleep]);
        assert_eq!(first.interface_state, InterfacePowerState::awake());

        let second = manager.tick(IDLE_SLEEP_TIMEOUT + Duration::from_secs(1));
        assert!(second.topology_flags.is_empty());
        assert_eq!(second.interface_state, InterfacePowerState::sleeping());
    }

    #[test]
    fn sleeping_transaction_waits_for_next_barrier_and_wakes_network() {
        let mut manager = PowerManager::new(Duration::from_secs(0));
        manager.tick(IDLE_SLEEP_TIMEOUT);

        let sleeping =
            manager.record_outbound_transaction(IDLE_SLEEP_TIMEOUT + Duration::from_millis(1_000));
        assert_eq!(sleeping.interface_state, InterfacePowerState::sleeping());
        assert!(!sleeping.wake_network);
        assert_eq!(manager.mode(), PowerMode::SynchronizedLowPower);

        let awake = manager.tick(IDLE_SLEEP_TIMEOUT + Duration::from_secs(5));
        assert!(awake.wake_network);
        assert_eq!(awake.interface_state, InterfacePowerState::awake());
        assert_eq!(manager.mode(), PowerMode::HighPower);
    }
}
