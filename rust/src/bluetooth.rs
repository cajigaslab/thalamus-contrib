use tokio::sync::OnceCell;
use btleplug::platform::Manager;

// btleplug's Manager::new() opens a fresh BlueZ D-Bus connection and spawns a detached task to
// pump it that is never torn down, so it must be created once and shared -- otherwise every node
// that creates its own leaks a connection that stays subscribed to a peripheral's notifications.
// All bluetooth-using nodes in the process share this single Manager/connection.
static MANAGER: OnceCell<Manager> = OnceCell::const_new();

pub async fn get_manager() -> btleplug::Result<Manager> {
  MANAGER.get_or_try_init(Manager::new).await.map(Clone::clone)
}
