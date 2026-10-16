//! MetaStore conformance suite (SPEC §14). Runs against SQLite always and
//! against Postgres when FLUX_TEST_POSTGRES_URL is set.

use std::sync::Arc;

use flux_core::{ChangesetId, ContentId};
use flux_store::*;

async fn backends() -> Vec<(&'static str, Arc<dyn MetaStore>)> {
    let mut out: Vec<(&'static str, Arc<dyn MetaStore>)> = vec![("sqlite", open_meta("sqlite::memory:").await.unwrap())];
    if let Ok(url) = std::env::var("FLUX_TEST_POSTGRES_URL") {
        out.push(("postgres", open_meta(&url).await.unwrap()));
    }
    out
}

fn unique(name: &str) -> String {
    format!("{name}-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos())
}

fn cs(n: u8, parents: Vec<ChangesetId>, gen_num: u64) -> NewChangeset {
    NewChangeset { id: ChangesetId::hash(&[n]), parents, gen_num, body: vec![n; 3], derived: vec![], git_sha1: None }
}

#[tokio::test]
async fn repos() {
    for (label, s) in backends().await {
        let name = unique("repo");
        let id = s.create_repo(&name).await.unwrap();
        assert!(matches!(s.create_repo(&name).await, Err(StoreError::RepoExists(_))), "{label}");
        assert_eq!(s.get_repo(&name).await.unwrap().unwrap().id, id, "{label}");
        assert!(s.get_repo("nope-nope").await.unwrap().is_none());
    }
}

#[tokio::test]
async fn changesets_and_log() {
    for (label, s) in backends().await {
        let repo = s.create_repo(&unique("cs")).await.unwrap();
        let a = cs(1, vec![], 1);
        let b = cs(2, vec![a.id], 2);
        let c = cs(3, vec![a.id], 2);
        let seqs = s.put_changesets(repo, &[a.clone(), b.clone()]).await.unwrap();
        assert_eq!(seqs, vec![1, 2], "{label}");
        // Idempotent re-insert keeps the original position; new ones continue.
        let seqs = s.put_changesets(repo, &[b.clone(), c.clone()]).await.unwrap();
        assert_eq!(seqs, vec![2, 3], "{label}");

        let got = s.get_changesets(repo, &[c.id, ChangesetId::hash(b"missing"), a.id]).await.unwrap();
        assert_eq!(got[0].as_ref().unwrap().gen_num, 2);
        assert!(got[1].is_none());
        assert_eq!(got[2].as_ref().unwrap().body, vec![1; 3]);

        assert_eq!(s.get_gens(repo, &[b.id]).await.unwrap(), vec![Some(2)]);
        let tail = s.log_tail(repo, 1, 10).await.unwrap();
        assert_eq!(tail, vec![(2, b.id), (3, c.id)], "{label}");

        let mut kids = s.children(repo, &a.id).await.unwrap();
        kids.sort();
        let mut want = vec![b.id, c.id];
        want.sort();
        assert_eq!(kids, want);

        let hex = b.id.to_hex();
        assert_eq!(s.resolve_prefix(repo, &hex[..8], 2).await.unwrap(), vec![b.id], "{label}");
        assert_eq!(s.resolve_prefix(repo, &hex[..7], 2).await.unwrap(), vec![b.id], "{label}");
    }
}

#[tokio::test]
async fn ref_cas() {
    for (label, s) in backends().await {
        let repo = s.create_repo(&unique("refs")).await.unwrap();
        let a = ChangesetId::hash(b"a");
        let b = ChangesetId::hash(b"b");
        let log = || RefLogEntry { actor: "test".into(), reason: "test".into() };

        assert_eq!(
            s.cas_ref(repo, b"main", RefExpect::Absent, Some(a), log()).await.unwrap(),
            CasOutcome::Updated { version: 1 }
        );
        assert!(
            matches!(
                s.cas_ref(repo, b"main", RefExpect::Absent, Some(b), log()).await.unwrap(),
                CasOutcome::Mismatch { current: Some(RefValue { version: 1, .. }) }
            ),
            "{label}"
        );
        assert!(matches!(
            s.cas_ref(repo, b"main", RefExpect::Version(7), Some(b), log()).await.unwrap(),
            CasOutcome::Mismatch { .. }
        ));
        assert_eq!(
            s.cas_ref(repo, b"main", RefExpect::Target(a), Some(b), log()).await.unwrap(),
            CasOutcome::Updated { version: 2 }
        );
        assert_eq!(s.get_ref(repo, b"main").await.unwrap(), Some(RefValue { target: b, version: 2 }));

        s.cas_ref(repo, b"scratch/x", RefExpect::Absent, Some(a), log()).await.unwrap();
        s.cas_ref(repo, b"scratch/y", RefExpect::Absent, Some(a), log()).await.unwrap();
        let listed = s.list_refs(repo, b"scratch/", 100).await.unwrap();
        assert_eq!(listed.iter().map(|(n, _)| n.as_slice()).collect::<Vec<_>>(), vec![&b"scratch/x"[..], b"scratch/y"]);

        assert_eq!(s.cas_ref(repo, b"scratch/x", RefExpect::Version(1), None, log()).await.unwrap(), CasOutcome::Deleted);
        assert!(s.get_ref(repo, b"scratch/x").await.unwrap().is_none());
    }
}

#[tokio::test]
async fn concurrent_cas_is_linearizable() {
    for (label, s) in backends().await {
        let repo = s.create_repo(&unique("race")).await.unwrap();
        let base = ChangesetId::hash(b"base");
        s.cas_ref(repo, b"main", RefExpect::Absent, Some(base), RefLogEntry { actor: "t".into(), reason: "init".into() })
            .await
            .unwrap();
        let mut handles = vec![];
        for i in 0..16u8 {
            let s = s.clone();
            handles.push(tokio::spawn(async move {
                s.cas_ref(
                    repo,
                    b"main",
                    RefExpect::Version(1),
                    Some(ChangesetId::hash(&[i])),
                    RefLogEntry { actor: "t".into(), reason: "race".into() },
                )
                .await
                .unwrap()
            }));
        }
        let mut wins = 0;
        for h in handles {
            if matches!(h.await.unwrap(), CasOutcome::Updated { .. }) {
                wins += 1;
            }
        }
        assert_eq!(wins, 1, "{label}: exactly one CAS must win");
    }
}

#[tokio::test]
async fn derived_content_git() {
    for (_label, s) in backends().await {
        let repo = s.create_repo(&unique("misc")).await.unwrap();
        let a = ChangesetId::hash(b"a");
        s.put_derived(repo, 1, &[(a, vec![9; 32])]).await.unwrap();
        assert_eq!(s.get_derived(repo, 1, &[a]).await.unwrap(), vec![Some(vec![9; 32])]);
        assert_eq!(s.get_derived(repo, 2, &[a]).await.unwrap(), vec![None]);

        let cid = ContentId::hash(b"x");
        let meta = ContentMeta { size: 1, git_sha1: [1; 20], sha256: [2; 32], is_binary: true };
        s.put_content_meta(&[(cid, meta)]).await.unwrap();
        assert_eq!(s.get_content_meta(&[cid]).await.unwrap(), vec![Some(meta)]);

        assert_eq!(s.git_pack_get(repo).await.unwrap(), None);
        let mut pack = GitPack { content: ContentId::hash(b"pack"), objects: 3, tips: vec![a], created_ms: 5 };
        s.git_pack_put(repo, &pack).await.unwrap();
        assert_eq!(s.git_pack_get(repo).await.unwrap(), Some(pack.clone()));
        pack.objects = 4;
        s.git_pack_put(repo, &pack).await.unwrap();
        assert_eq!(s.git_pack_get(repo).await.unwrap().unwrap().objects, 4, "replaced");
        s.git_map_put(repo, &[(a, [7; 20])]).await.unwrap();
        assert_eq!(s.git_to_cs(repo, &[[7; 20], [8; 20]]).await.unwrap(), vec![Some(a), None]);
        assert_eq!(s.cs_to_git(repo, &[a]).await.unwrap(), vec![Some([7; 20])]);
    }
}

#[tokio::test]
async fn accounts_orgs_teams_and_grants() {
    for (name, s) in backends().await {
        let alice = s
            .create_account(&NewAccount {
                name: "alice".into(),
                email: Some("alice@example.com".into()),
                display_name: "Alice".into(),
                password_hash: Some("$argon2id$x".into()),
                is_admin: false,
            })
            .await
            .unwrap();
        let bob = s.create_account(&NewAccount { name: "bob".into(), ..Default::default() }).await.unwrap();
        assert!(s.create_account(&NewAccount { name: "alice".into(), ..Default::default() }).await.is_err(), "{name}: dup name");
        let dup_email = NewAccount { name: "carol".into(), email: Some("alice@example.com".into()), ..Default::default() };
        assert!(matches!(s.create_account(&dup_email).await, Err(StoreError::Conflict(_))), "{name}: dup email");
        let (acct, hash) = s.get_account("alice@example.com").await.unwrap().unwrap();
        assert_eq!((acct.user.id, acct.display_name.as_str(), hash.as_deref()), (alice.id, "Alice", Some("$argon2id$x")));
        assert_eq!(s.user_count().await.unwrap(), 2);

        // Tokens: expiry, disabled accounts, listing and revocation by id.
        let (t1, t2) = ([1u8; 32], [2u8; 32]);
        s.add_token_ext(alice.id, &t1, "laptop", "api", "read", None).await.unwrap();
        s.add_token_ext(alice.id, &t2, "web", "session", "all", Some(1_000)).await.unwrap();
        assert_eq!(s.token_auth(&t1, 5_000).await.unwrap().unwrap().scopes, "read");
        assert!(s.token_auth(&t2, 5_000).await.unwrap().is_none(), "{name}: expired");
        assert!(s.token_auth(&t2, 500).await.unwrap().is_some());
        s.update_account(alice.id, &AccountUpdate { disabled: Some(true), ..Default::default() }).await.unwrap();
        assert!(s.token_auth(&t1, 5_000).await.unwrap().is_none(), "{name}: disabled account");
        s.update_account(alice.id, &AccountUpdate { disabled: Some(false), ..Default::default() }).await.unwrap();
        let toks = s.list_tokens(alice.id).await.unwrap();
        assert_eq!(toks.len(), 2);
        let laptop = toks.iter().find(|t| t.label == "laptop").unwrap();
        assert!(s.revoke_token(alice.id, &laptop.id).await.unwrap());
        assert!(!s.revoke_token(bob.id, &toks[0].id).await.unwrap(), "{name}: other users' tokens are untouchable");

        // Organizations share the user namespace.
        assert!(matches!(s.create_org("bob", "", alice.id).await, Err(StoreError::Conflict(_))));
        let acme = s.create_org("acme", "Acme Inc", alice.id).await.unwrap();
        assert!(s.create_account(&NewAccount { name: "acme".into(), ..Default::default() }).await.is_err());
        s.set_org_member(acme.id, bob.id, Some("member")).await.unwrap();
        assert_eq!(s.org_members(acme.id).await.unwrap().len(), 2);
        assert_eq!(s.orgs_of_user(bob.id).await.unwrap()[0].1, "member");

        // Repositories, teams and grants.
        let repo = s.create_repo_owned("acme/api", "org", acme.id, "private", "none").await.unwrap();
        let access = s.repo_access(repo).await.unwrap().unwrap();
        assert_eq!((access.owner_kind.as_str(), access.visibility.as_str()), ("org", "private"));
        let facts = s.authz_facts(repo, bob.id).await.unwrap();
        assert_eq!((facts.org_role.as_deref(), facts.org_base_role.as_deref()), (Some("member"), Some("read")));
        assert!(facts.grants.is_empty());
        let team = s.create_team(acme.id, "backend", "").await.unwrap();
        s.set_team_member(team.id, bob.id, true).await.unwrap();
        s.set_repo_grant(repo, "team", team.id, Some("write")).await.unwrap();
        s.set_repo_grant(repo, "user", bob.id, Some("triage")).await.unwrap();
        let mut g = s.authz_facts(repo, bob.id).await.unwrap().grants;
        g.sort();
        assert_eq!(g, vec!["triage".to_string(), "write".to_string()]);
        assert_eq!(s.repo_grants(repo).await.unwrap().len(), 2);
        // Leaving the org drops team membership too.
        s.set_org_member(acme.id, bob.id, None).await.unwrap();
        let facts = s.authz_facts(repo, bob.id).await.unwrap();
        assert_eq!((facts.org_role, facts.grants), (None, vec!["triage".to_string()]));
        s.delete_team(team.id).await.unwrap();
        assert!(s.list_teams(acme.id).await.unwrap().is_empty());

        s.audit_append("alice", "org.create", "acme", "").await.unwrap();
        assert_eq!(s.audit_list(None, 10).await.unwrap()[0].action, "org.create");
    }
}

#[tokio::test]
async fn issues_share_numbers_with_reviews() {
    for (label, s) in backends().await {
        let repo = s.create_repo(&unique("iss")).await.unwrap();
        let head = ChangesetId::hash(b"h");
        let r = s.create_review(repo, "r", "", "ann", head, b"main").await.unwrap();
        assert_eq!(r.number, 1, "{label}");
        let i = s
            .create_issue(
                repo,
                &NewIssue { title: "bug".into(), body: "it Breaks".into(), author: "bob".into(), ..Default::default() },
            )
            .await
            .unwrap();
        assert_eq!(i.number, 2, "{label}");
        assert_eq!(i.state, "open");
        let r2 = s.create_review(repo, "r2", "", "ann", head, b"main").await.unwrap();
        assert_eq!(r2.number, 3, "{label}");
        // Imports keep their numbers; taken numbers conflict.
        let imp = s
            .create_issue(repo, &NewIssue { number: Some(10), title: "old".into(), author: "x".into(), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(imp.number, 10);
        assert!(matches!(
            s.create_issue(repo, &NewIssue { number: Some(1), title: "dup".into(), author: "x".into(), ..Default::default() })
                .await,
            Err(StoreError::Conflict(_))
        ));
        assert_eq!(s.next_item_number(repo).await.unwrap(), 11);
        assert_eq!(s.item_kind(repo, 1).await.unwrap(), Some("review"));
        assert_eq!(s.item_kind(repo, 2).await.unwrap(), Some("issue"));
        assert_eq!(s.item_kind(repo, 5).await.unwrap(), None);
        let imported = s
            .import_review(
                repo,
                &ImportedReview {
                    number: 20,
                    title: "pr".into(),
                    body: "".into(),
                    author: "gh".into(),
                    head,
                    target: b"main".to_vec(),
                    state: "landed".into(),
                    landed: Some(head),
                    draft: false,
                    created_ms: 5,
                    updated_ms: 6,
                },
            )
            .await
            .unwrap();
        assert_eq!((imported.number, imported.created_ms), (20, 5));

        // Comments, labels, people and filters.
        let c = s.add_issue_event(repo, 2, "comment", "ann", "me too", None, Some("gh-1")).await.unwrap();
        let again = s.add_issue_event(repo, 2, "comment", "ann", "me too", None, Some("gh-1")).await.unwrap();
        assert_eq!(c.seq, again.seq, "{label}: external ids are idempotent");
        assert_eq!(s.get_issue(repo, 2).await.unwrap().unwrap().comments, 1);
        s.put_label(repo, &Label { name: "bug".into(), color: "ff0000".into(), description: "".into() }).await.unwrap();
        s.set_item_labels(repo, 2, &["bug".into()]).await.unwrap();
        s.set_item_people(repo, 2, "assignee", &["carol".into()]).await.unwrap();
        let q = |f: fn(&mut IssueQuery)| {
            let mut q = IssueQuery { limit: 50, ..Default::default() };
            f(&mut q);
            q
        };
        assert_eq!(s.list_issues(repo, &q(|q| q.label = Some("bug".into()))).await.unwrap().1, 1);
        assert_eq!(s.list_issues(repo, &q(|q| q.assignee = Some("carol".into()))).await.unwrap().1, 1);
        assert_eq!(s.list_issues(repo, &q(|q| q.text = Some("BREAKS".into()))).await.unwrap().1, 1);
        assert_eq!(s.list_issues(repo, &q(|q| q.state = Some("open".into()))).await.unwrap().1, 2);
        s.update_issue(repo, 2, &IssueUpdate { state: Some("closed".into()), ..Default::default() }).await.unwrap();
        assert_eq!(s.issue_counts(repo).await.unwrap(), (1, 1));
        assert!(s.get_issue(repo, 2).await.unwrap().unwrap().closed_ms.is_some());
        s.rename_label(repo, "bug", &Label { name: "defect".into(), color: "00ff00".into(), description: "".into() })
            .await
            .unwrap();
        assert_eq!(s.item_labels(repo, &[2]).await.unwrap(), vec![(2, "defect".to_string())]);
        assert!(s.delete_issue_event(repo, c.seq).await.unwrap());
        assert_eq!(s.get_issue(repo, 2).await.unwrap().unwrap().comments, 0);

        let m = s.create_milestone(repo, None, "v1", "", None).await.unwrap();
        s.update_issue(repo, 10, &IssueUpdate { milestone: Some(Some(m.number)), ..Default::default() }).await.unwrap();
        let ms = s.list_milestones(repo).await.unwrap();
        assert_eq!((ms[0].open_items, ms[0].closed_items), (1, 0), "{label}");
    }
}

#[tokio::test]
async fn statuses_hooks_keys_notifications() {
    for (label, s) in backends().await {
        let repo = s.create_repo(&unique("st")).await.unwrap();
        let c = ChangesetId::hash(b"c");
        let st = |state: &str| CommitStatus {
            context: "ci/test".into(),
            state: state.into(),
            description: "".into(),
            target_url: "".into(),
            creator: "bot".into(),
            created_ms: 0,
            updated_ms: 0,
        };
        s.put_status(repo, &c, &st("pending")).await.unwrap();
        s.put_status(repo, &c, &st("success")).await.unwrap();
        let all = s.statuses(repo, &c).await.unwrap();
        assert_eq!(all.len(), 1, "{label}");
        assert_eq!(all[0].state, "success");
        assert_eq!(s.statuses_many(repo, &[c, ChangesetId::hash(b"z")]).await.unwrap().len(), 1);

        let h = s
            .create_webhook(&Webhook {
                id: 0,
                scope_kind: "repo".into(),
                scope_id: repo.0,
                url: "http://x".into(),
                secret: "s".into(),
                events: vec!["push".into(), "issues".into()],
                active: true,
                created_ms: 1,
            })
            .await
            .unwrap();
        assert_eq!(s.list_webhooks("repo", repo.0).await.unwrap()[0].events, vec!["push", "issues"]);
        let d = s.enqueue_delivery(h.id, "push", "g1", "{}").await.unwrap();
        let claimed = s.claim_deliveries(i64::MAX / 2, i64::MAX / 2 + 1000, 10).await.unwrap();
        assert!(claimed.iter().any(|x| x.id == d), "{label}");
        let again = s.claim_deliveries(i64::MAX / 2, i64::MAX / 2 + 1000, 10).await.unwrap();
        assert!(!again.iter().any(|x| x.id == d), "{label}: leased deliveries are not handed out twice");
        s.finish_delivery(d, "ok", 0, 200, "fine", 12).await.unwrap();
        assert_eq!(s.get_delivery(d).await.unwrap().unwrap().state, "ok");

        let u = s.create_user(&unique("u"), false).await.unwrap();
        let fp = unique("SHA256:");
        s.add_ssh_key(u.id, "laptop", &fp, "ssh-ed25519 AAAA").await.unwrap();
        assert!(matches!(s.add_ssh_key(u.id, "again", &fp, "x").await, Err(StoreError::Conflict(_))));
        assert_eq!(s.ssh_key_by_fingerprint(&fp).await.unwrap().unwrap().user_id, u.id);

        s.notify(u.id, repo, 3, "issue", "mention", "t").await.unwrap();
        s.notify(u.id, repo, 3, "issue", "comment", "t").await.unwrap();
        assert_eq!(s.unread_count(u.id).await.unwrap(), 1, "{label}");
        s.mark_read(u.id, None).await.unwrap();
        assert_eq!(s.unread_count(u.id).await.unwrap(), 0);
        assert_eq!(s.notifications(u.id, false, 10).await.unwrap().len(), 1);

        s.link_identity("github", "42", u.id, "octo").await.unwrap();
        let other = s.create_user(&unique("v"), false).await.unwrap();
        assert!(s.link_identity("github", "42", other.id, "octo").await.is_err(), "{label}");
        assert_eq!(s.identity("github", "42").await.unwrap().unwrap().user_id, u.id);
        s.set_totp(u.id, Some("ABC"), &["h1".into()]).await.unwrap();
        assert_eq!(s.totp(u.id).await.unwrap(), (Some("ABC".to_string()), vec!["h1".to_string()]));
    }
}

#[tokio::test]
async fn ci_queue_claims_once() {
    for (label, s) in backends().await {
        let repo = s.create_repo(&unique("ci")).await.unwrap();
        let run = CiRun {
            id: 0,
            repo_id: repo,
            workflow: "ci".into(),
            event: "push".into(),
            ref_name: "main".into(),
            cs: ChangesetId::hash(b"c"),
            review: None,
            state: "queued".into(),
            actor: "a".into(),
            created_ms: 0,
            updated_ms: 0,
        };
        let jobs = vec![
            NewCiJob { name: "build".into(), needs: vec![], labels: vec!["linux".into()], spec: "{}".into() },
            NewCiJob { name: "deploy".into(), needs: vec!["build".into()], labels: vec![], spec: "{}".into() },
        ];
        let (run, jobs) = s.create_ci_run(&run, &jobs).await.unwrap();
        assert_eq!(jobs[1].state, "blocked", "{label}");
        assert!(s.claim_ci_job("r1", &["mac".into()], 10, 100).await.unwrap().is_none_or(|j| j.run_id != run.id));
        let j = s.claim_ci_job("r1", &["linux".into()], 10, 100).await.unwrap().unwrap();
        assert_eq!(j.id, jobs[0].id);
        let none = s.claim_ci_job("r2", &["linux".into()], 10, 100).await.unwrap();
        assert!(none.is_none_or(|x| x.run_id != run.id), "{label}");
        s.append_ci_log(j.id, 1, b"hello ").await.unwrap();
        s.append_ci_log(j.id, 2, b"world").await.unwrap();
        assert_eq!(s.ci_log(j.id, 1).await.unwrap(), vec![(2, b"world".to_vec())]);
        assert!(!s.set_ci_job(j.id, "intruder", "success", Some(0), 0, 20).await.unwrap());
        assert!(s.set_ci_job(j.id, "r1", "success", Some(0), 0, 20).await.unwrap());
        assert_eq!(s.get_ci_job(j.id).await.unwrap().unwrap().finished_ms, Some(20));
    }
}
