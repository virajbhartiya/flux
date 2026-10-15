//! Accounts, organizations, teams and repository roles, end to end.

use flux_client::Client;
use flux_server::auth::AuthMode;
use flux_server::rbac::Role;

#[tokio::test(flavor = "multi_thread")]
async fn org_repositories_follow_roles() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = flux_server::StorageConfig::data_dir(dir.path()).unwrap();
    let flux = flux_server::open_with(&cfg, AuthMode::Token).await.unwrap();

    // The first account becomes a site admin; sign-up is then closed.
    let root = flux.sign_up("root", Some("root@x.org"), "Root", "correct horse battery").await.unwrap();
    assert!(root.is_admin);
    assert!(flux.sign_up("eve", None, "", "correct horse battery").await.is_err(), "sign-up closed");
    let (_, root_session) = flux.password_login("root@x.org", "correct horse battery", "test").await.unwrap();
    let root_p = flux.principal(Some(&root_session)).await.unwrap();
    for name in ["alice", "bob", "carol", "dave"] {
        flux.admin_create_account(&root_p, name, None, Some("correct horse battery"), false).await.unwrap();
    }
    let session = |name: &'static str| {
        let flux = flux.clone();
        async move {
            let (_, t) = flux.password_login(name, "correct horse battery", "test").await.unwrap();
            (flux.principal(Some(&t)).await.unwrap(), t)
        }
    };
    let (alice, _) = session("alice").await;
    let (bob, _) = session("bob").await;
    let (carol, carol_token) = session("carol").await;
    let (dave, _) = session("dave").await;
    let anon = flux.principal(None).await.unwrap();

    // alice owns acme; bob is a member; carol is an outside collaborator.
    let acme = flux.create_org(&alice, "acme", "Acme").await.unwrap();
    flux.set_org_member(&alice, &acme, "bob", Some("member")).await.unwrap();
    assert!(flux.set_org_member(&bob, &acme, "dave", Some("member")).await.is_err(), "members can't add members");
    assert!(flux.create_repo_as(&bob, "acme/api", None).await.is_err(), "only owners create org repos");
    let api = flux.create_repo_as(&alice, "acme/api", None).await.unwrap();
    assert!(flux.create_repo_as(&bob, "alice/x", None).await.is_err(), "not under someone else's name");
    let bobs = flux.create_repo_as(&bob, "bob/notes", None).await.unwrap();

    // Private by default: owners admin, members get the base role (read),
    // everyone else sees nothing.
    assert_eq!(flux.repo_role(&alice, api).await.unwrap(), Role::Admin);
    assert_eq!(flux.repo_role(&bob, api).await.unwrap(), Role::Read);
    assert_eq!(flux.repo_role(&dave, api).await.unwrap(), Role::None);
    assert!(matches!(flux.authorize(&dave, api, Role::Read).await, Err(flux_server::core::FluxError::NotFound(_))));
    assert!(matches!(flux.authorize(&anon, api, Role::Read).await, Err(flux_server::core::FluxError::Unauthenticated(_))));
    assert_eq!(flux.repo_role(&bob, bobs).await.unwrap(), Role::Admin, "your own repository");
    assert_eq!(flux.repo_role(&root_p, bobs).await.unwrap(), Role::Admin, "site admins");

    // Teams and collaborators raise roles; the highest grant wins.
    let backend = flux.create_team(&alice, &acme, "backend", "").await.unwrap();
    assert!(flux.set_team_member(&alice, &acme, &backend, "carol", true).await.is_err(), "teams need org members");
    flux.set_team_member(&alice, &acme, &backend, "bob", true).await.unwrap();
    flux.set_grant(&alice, api, "acme/api", "team", "backend", Some(Role::Write)).await.unwrap();
    assert_eq!(flux.repo_role(&bob, api).await.unwrap(), Role::Write);
    flux.set_grant(&alice, api, "acme/api", "user", "carol", Some(Role::Triage)).await.unwrap();
    assert_eq!(flux.repo_role(&carol, api).await.unwrap(), Role::Triage);
    assert!(flux.set_grant(&bob, api, "acme/api", "user", "dave", Some(Role::Read)).await.is_err(), "grants need admin");

    // Base role changes apply to every member.
    flux.update_org(&alice, &acme, "Acme", "", "none").await.unwrap();
    flux.set_team_member(&alice, &acme, &backend, "bob", false).await.unwrap();
    assert_eq!(flux.repo_role(&bob, api).await.unwrap(), Role::None);

    // Visibility: internal means any signed-in user may read.
    flux.set_visibility(&alice, api, "acme/api", "internal").await.unwrap();
    assert_eq!(flux.repo_role(&dave, api).await.unwrap(), Role::Read);
    assert_eq!(flux.repo_role(&anon, api).await.unwrap(), Role::None);
    flux.set_visibility(&alice, api, "acme/api", "public").await.unwrap();
    assert_eq!(flux.repo_role(&anon, api).await.unwrap(), Role::Read);
    flux.set_visibility(&alice, api, "acme/api", "private").await.unwrap();

    // Token scopes cap the role.
    let read_only = flux.create_api_token(&alice, "ci", "read", Some(30)).await.unwrap();
    let alice_ro = flux.principal(Some(&read_only)).await.unwrap();
    assert_eq!(flux.repo_role(&alice_ro, api).await.unwrap(), Role::Read);
    assert!(flux.create_api_token(&alice_ro, "escalate", "all", None).await.is_err(), "no broader tokens from narrow ones");
    let tokens = flux.my_tokens(&alice).await.unwrap();
    let ci = tokens.iter().find(|t| t.label == "ci").unwrap();
    flux.revoke_my_token(&alice, &ci.id).await.unwrap();
    assert!(flux.principal(Some(&read_only)).await.is_err(), "revoked");

    // An organization keeps an owner.
    assert!(flux.set_org_member(&alice, &acme, "alice", None).await.is_err());
    flux.set_org_member(&alice, &acme, "bob", Some("owner")).await.unwrap();
    flux.set_org_member(&alice, &acme, "alice", None).await.unwrap();
    assert_eq!(flux.repo_role(&alice, api).await.unwrap(), Role::None);

    // Sign-in throttling, and disabled accounts lose their sessions.
    for _ in 0..10 {
        assert!(flux.password_login("dave", "wrong password!", "attacker").await.is_err());
    }
    assert!(flux.password_login("dave", "correct horse battery", "attacker").await.is_err(), "throttled");
    flux.admin_update_account(&root_p, "carol", None, Some(true)).await.unwrap();
    assert!(flux.principal(Some(&carol_token)).await.is_err(), "disabled");

    // Over gRPC: outsiders can't see private repositories at all.
    let (listener, addr) = flux_server::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
    tokio::spawn(flux_server::serve(flux.clone(), listener, std::future::pending()));
    let url = format!("http://{addr}");
    let (_, dave_token) = flux.password_login("dave", "correct horse battery", "other").await.unwrap();
    let dave_c = Client::connect_with_token(&url, "acme/api", Some(&dave_token)).await.unwrap();
    assert!(dave_c.list_refs(b"").await.is_err());
    assert!(!dave_c.list_repos().await.unwrap().iter().any(|r| r == "acme/api"));
    let (_, bob_token) = flux.password_login("bob", "correct horse battery", "other").await.unwrap();
    let bob_c = Client::connect_with_token(&url, "acme/api", Some(&bob_token)).await.unwrap();
    assert!(bob_c.list_refs(b"").await.is_ok(), "bob owns acme now");
    assert!(bob_c.list_repos().await.unwrap().iter().any(|r| r == "acme/api"));

    let audit = flux.meta.audit_list(None, 100).await.unwrap();
    for action in
        ["org.create", "repo.create", "repo.grant", "team.member", "repo.visibility", "token.revoke", "user.login_failed"]
    {
        assert!(audit.iter().any(|e| e.action == action), "audited: {action}");
    }
}
