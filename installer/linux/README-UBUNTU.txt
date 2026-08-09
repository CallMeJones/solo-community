Solo Community for Ubuntu 24.04
================================

Install:
  sudo apt install ./solo-<version>-ubuntu24.04-amd64.deb

Launch Solo from the application menu, or run:
  solo-tray

The package includes:
  /usr/bin/solo       command-line client and daemon
  /usr/bin/solo-tray  Desktop, tray, and daemon supervisor
  /usr/share/solo/models/all-MiniLM-L6-v2
                       pinned local semantic model (no first-use download)

Solo stores encrypted local data in your user account. Desktop secrets use
the Linux Secret Service keyring. The app never falls back to a plaintext
credential file.

To start Solo automatically when you sign in, enable autostart from the Solo
tray menu. Solo writes a per-user XDG entry under ~/.config/autostart.

Uninstall:
  sudo apt remove solo-memory

Your user data is intentionally left in place when the package is removed.
