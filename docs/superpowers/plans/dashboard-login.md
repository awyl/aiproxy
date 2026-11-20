# Dashboard login

- [x] RED: test anonymous pages and management APIs, wrong password, session reuse and revocation, and no-token startup.
- [x] GREEN: route dashboard HTML and management API through separate guards backed by the same bounded in-memory sessions. Keep existing global bearer access for API callers. Leave client routes outside both guards.
- [x] Remove page login fields; test existing controls with browser cookies and add shared sign-out. Document configuration and verify suites.

Work on the existing feature branch because the dashboard depends on uncommitted Skills changes there. No release/tag or push.
