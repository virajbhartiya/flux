//! Releases (notes and downloadable assets) and source archives.

use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use maud::{Markup, PreEscaped, html};

use super::forms::{Fields, done};
use super::markdown;
use super::pages::{RepoCtx, nav, parse_num, url_decode};
use super::view::{self, Tab, avatar, empty_state, enc, icon, page, time, url};
use super::{Req, WebError, WebResult, WebState};
use crate::rbac::Role;
use crate::repos::{ArchiveFormat, ReleaseInput, ReleaseJson};

fn base(ctx: &RepoCtx) -> String {
    format!("/{}/-", enc(&ctx.name))
}

fn download_url(ctx: &RepoCtx, tag: &str, name: &str) -> String {
    format!("{}/releases/download/{}/{}", base(ctx), enc(tag), enc(name))
}

/// Archive links for a revision: (zip, tar.gz).
pub fn archive_urls(ctx: &RepoCtx, rev: &str) -> (String, String) {
    let b = format!("{}/archive/{}", base(ctx), enc(rev));
    (format!("{b}.zip"), format!("{b}.tar.gz"))
}

pub async fn releases_get(web: &WebState, req: &Req, ctx: &RepoCtx, args: &[String]) -> WebResult {
    match args {
        [] => list(web, req, ctx).await,
        [n] if n == "new" => form(web, req, ctx, None).await,
        [id, e] if e == "edit" => form(web, req, ctx, Some(parse_num(id)?)).await,
        [d, rest @ ..] if d == "download" && rest.len() >= 2 => {
            let decoded: Vec<String> = rest.iter().map(|s| url_decode(s)).collect();
            let (name, tag) = decoded.split_last().expect("checked length");
            download(web, req, ctx, &tag.join("/"), name).await
        }
        _ => Err(WebError::NotFound("no such page".into())),
    }
}

fn release_card(ctx: &RepoCtx, r: &ReleaseJson, latest: bool, can_write: bool) -> Markup {
    let b = base(ctx);
    let tag_ref = format!("tags/{}", r.tag);
    let rev = if r.draft { r.target.clone().unwrap_or_else(|| tag_ref.clone()) } else { tag_ref.clone() };
    let (zip, tgz) = archive_urls(ctx, &rev);
    html! {
        article.card.release id=(format!("release-{}", r.id)) {
            div.release-head {
                div.release-title {
                    h2 { (if r.name.is_empty() { &r.tag } else { &r.name }) }
                    @if latest { span.pill.small.ok { "Latest" } }
                    @if r.prerelease { span.pill.small.warn { "Pre-release" } }
                    @if r.draft { span.pill.small { "Draft" } }
                }
                div.release-meta.muted.small {
                    @if r.draft { span { (icon("tag")) " " code { (r.tag) } " (not yet tagged)" } }
                    @else { a.branch href=(url(&ctx.name, "tree", &tag_ref, None)) { (icon("tag")) (r.tag) } }
                    @if let Some(t) = &r.target { a.sha href=(url(&ctx.name, "commit", t, None)) { (&t[..10.min(t.len())]) } }
                    span { (avatar(&r.author, "", false)) " " (r.author) }
                    span { @match r.published_at { Some(p) => { "released " (time(p)) } None => { "created " (time(r.created_at)) } } }
                }
                @if can_write {
                    a.btn.small href=(format!("{b}/releases/{}/edit", r.id)) { (icon("edit")) "Edit" }
                }
            }
            div.release-body {
                @if r.body.trim().is_empty() { p.muted { "No release notes." } }
                @else { article.markdown { (PreEscaped(markdown::render(&r.body, None))) } }
            }
            details.assets open {
                summary { "Assets" span.count { (r.assets.len() + 2) } }
                ul.asset-list {
                    @for a in &r.assets {
                        li {
                            (icon("archive"))
                            a.asset-name href=(download_url(ctx, &r.tag, &a.name)) download data-no-swap { (a.name) }
                            span.muted.small { (view::size(a.size as u64)) }
                            span.muted.small.hide-sm { (a.downloads) " download" @if a.downloads != 1 { "s" } }
                        }
                    }
                    li { (icon("code")) a.asset-name href=(zip) download data-no-swap { "Source code (zip)" } }
                    li { (icon("code")) a.asset-name href=(tgz) download data-no-swap { "Source code (tar.gz)" } }
                }
            }
        }
    }
}

async fn list(web: &WebState, req: &Req, ctx: &RepoCtx) -> WebResult {
    let releases = web.flux.list_releases_as(&req.principal, ctx.id).await?;
    let latest = releases.iter().find(|r| !r.draft && !r.prerelease).map(|r| r.id);
    let can_write = ctx.can(Role::Write);
    let b = base(ctx);
    let body = html! {
        div.container {
            div.page-head.row {
                div { h1 { "Releases" } p.muted { "Tagged versions with notes and downloadable files." } }
                div.spacer {}
                @if can_write { a.btn.primary href=(format!("{b}/releases/new")) { (icon("plus")) "New release" } }
            }
            @if releases.is_empty() {
                (empty_state("No releases yet", html! {
                    p { "Releases package a tag with notes and files people can download." }
                    @if can_write { p { a.btn.primary href=(format!("{b}/releases/new")) { "Draft the first release" } } }
                }))
            }
            div.release-list {
                @for r in &releases { (release_card(ctx, r, Some(r.id) == latest, can_write)) }
            }
        }
    };
    let rev = ctx.default_branch().unwrap_or("main").to_string();
    let n = nav(web, req, ctx, Tab::Releases, &rev).await;
    Ok(page(web, req, &format!("Releases · {}", ctx.name), Some(n), body))
}

async fn form(web: &WebState, req: &Req, ctx: &RepoCtx, id: Option<i64>) -> WebResult {
    ctx.require(Role::Write)?;
    let flux = &web.flux;
    let existing = match id {
        Some(id) => Some(
            flux.list_releases_as(&req.principal, ctx.id)
                .await?
                .into_iter()
                .find(|r| r.id == id)
                .ok_or_else(|| WebError::NotFound("release not found".into()))?,
        ),
        None => None,
    };
    let b = base(ctx);
    let default = ctx.default_branch().unwrap_or("main").to_string();
    let tags: Vec<&str> = ctx.refs.iter().filter_map(|(n, _)| n.strip_prefix("tags/")).collect();
    let body = html! {
        div.container.narrow {
            div.page-head {
                h1 { @if existing.is_some() { "Edit release" } @else { "New release" } }
                p.muted { "Publishing a release creates its tag if it doesn't exist yet." }
            }
            @match &existing {
                None => {
                    form.card.form method="post" action=(format!("{b}/releases")) enctype="multipart/form-data" {
                        div.row2 {
                            label { "Tag" span.hint { "An existing tag, or a new one." }
                                input.mono type="text" name="tag" required placeholder="v1.0.0" list="release-tags" autofocus;
                                datalist #release-tags { @for t in &tags { option value=(t) {} } }
                            }
                            label { "Target" span.hint { "Where a new tag points." }
                                select name="target" { @for (n, _) in ctx.refs.iter().filter(|(n, _)| !n.starts_with("tags/")) { option value=(n) selected[*n == default] { (n) } } }
                            }
                        }
                        label { "Title" input type="text" name="name" placeholder="Defaults to the tag"; }
                        label { "Release notes " span.muted { "(Markdown)" } textarea name="body" rows="10" placeholder="What's new, what changed, what to watch out for…" {} }
                        label { "Attach files" span.hint { "Binaries, checksums, anything people should download." } input type="file" name="file" multiple; }
                        div.checks-row {
                            label.check { input type="checkbox" name="prerelease" value="1"; " This is a pre-release" }
                            label.check { input type="checkbox" name="draft" value="1"; " Save as draft" }
                        }
                        div.form-actions { a.btn href=(format!("{b}/releases")) { "Cancel" } button.btn.primary type="submit" { (icon("tag")) "Publish release" } }
                    }
                }
                Some(r) => {
                    form.card.form method="post" action=(format!("{b}/releases/{}", r.id)) {
                        p.muted { (icon("tag")) " " code { (r.tag) } }
                        label { "Title" input type="text" name="name" value=(r.name); }
                        label { "Release notes " span.muted { "(Markdown)" } textarea name="body" rows="12" { (r.body) } }
                        div.checks-row {
                            label.check { input type="checkbox" name="prerelease" value="1" checked[r.prerelease]; " This is a pre-release" }
                            label.check { input type="checkbox" name="draft" value="1" checked[r.draft]; " Draft (not published)" }
                        }
                        div.form-actions { a.btn href=(format!("{b}/releases")) { "Cancel" } button.btn.primary type="submit" { (if r.draft { "Save" } else { "Update release" }) } }
                    }
                    div.card {
                        div.card-head { (icon("archive")) "Assets" span.count { (r.assets.len()) } }
                        @for a in &r.assets {
                            div.list-row {
                                span.list-icon { (icon("archive")) }
                                div.list-main { a href=(download_url(ctx, &r.tag, &a.name)) download data-no-swap { strong { (a.name) } } div.muted.small { (view::size(a.size as u64)) " · " (a.content_type) " · " (a.downloads) " downloads" } }
                                form method="post" action=(format!("{b}/releases/{}/assets/{}/delete", r.id, a.id)) data-confirm=(format!("Delete {}?", a.name)) {
                                    button.btn.small.danger type="submit" aria-label=(format!("Delete {}", a.name)) { (icon("trash")) }
                                }
                            }
                        }
                        form.form.row-form method="post" action=(format!("{b}/releases/{}/assets", r.id)) enctype="multipart/form-data" {
                            label.grow { "Upload files" input type="file" name="file" multiple required; }
                            div.form-actions { button.btn type="submit" { (icon("upload")) "Upload" } }
                        }
                    }
                    div.card.danger-zone {
                        div.list-row {
                            div.list-main { strong { "Delete this release" } div.muted.small { "The tag stays; only the release, its notes and files go." } }
                            form method="post" action=(format!("{b}/releases/{}/delete", r.id)) data-confirm="Delete this release?" {
                                button.btn.small.danger type="submit" { (icon("trash")) "Delete release" }
                            }
                        }
                    }
                }
            }
        }
    };
    let n = nav(web, req, ctx, Tab::Releases, &default).await;
    Ok(page(
        web,
        req,
        &format!("{} · {}", if existing.is_some() { "Edit release" } else { "New release" }, ctx.name),
        Some(n),
        body,
    ))
}

/// File downloads never render as this origin.
fn attachment(mut r: Response, ctype: &str, filename: &str) -> Response {
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_str(ctype).unwrap_or(HeaderValue::from_static("application/octet-stream")));
    let safe: String = filename.chars().filter(|c| !c.is_control() && *c != '"' && *c != '\\').collect();
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{safe}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'; sandbox"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
    r
}

async fn download(web: &WebState, req: &Req, ctx: &RepoCtx, tag: &str, name: &str) -> WebResult {
    let (a, data) = web.flux.download_asset(&req.principal, ctx.id, tag, name).await?;
    Ok(attachment(data.into_response(), &a.content_type, &a.name))
}

/// `/{repo}/-/archive/{rev}.zip` and `.tar.gz`.
pub async fn archive(web: &WebState, _req: &Req, ctx: &RepoCtx, args: &[String]) -> WebResult {
    let file = url_decode(&args.join("/"));
    if ctx.config.has_rules() && !ctx.can(Role::Admin) {
        return Err(WebError::Forbidden("archives are unavailable when path rules restrict reading".into()));
    }
    let (rev, format, ext) = if let Some(r) = file.strip_suffix(".tar.gz") {
        (r, ArchiveFormat::TarGz, "tar.gz")
    } else if let Some(r) = file.strip_suffix(".tgz") {
        (r, ArchiveFormat::TarGz, "tar.gz")
    } else if let Some(r) = file.strip_suffix(".zip") {
        (r, ArchiveFormat::Zip, "zip")
    } else {
        return Err(WebError::NotFound("archives are {rev}.zip or {rev}.tar.gz".into()));
    };
    let id = web.flux.resolve_rev(ctx.id, rev).await?;
    let short = ctx.name.rsplit('/').next().unwrap_or(&ctx.name);
    let shown = rev.strip_prefix("tags/").unwrap_or(rev);
    let shown = if shown.len() == 64 { &shown[..10] } else { shown };
    let safe: String = shown.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '-' }).collect();
    let prefix = format!("{short}-{safe}");
    let data = web.flux.archive(ctx.id, &id, &prefix, format).await?;
    let ctype = if ext == "zip" { "application/zip" } else { "application/gzip" };
    Ok(attachment(data.into_response(), ctype, &format!("{prefix}.{ext}")))
}

pub async fn post(web: &WebState, req: &Req, ctx: &RepoCtx, s: &[&str], f: Fields) -> WebResult {
    let flux = &web.flux;
    let who = &req.principal;
    let b = base(ctx);
    let list = format!("{b}/releases");
    let upload = |id: i64, f: &Fields| {
        let files: Vec<(String, String, bytes::Bytes)> = f
            .files
            .iter()
            .filter(|u| u.field == "file")
            .map(|u| (u.filename.clone(), u.content_type.clone(), u.data.clone()))
            .collect();
        async move {
            let mut n = 0;
            for (name, ctype, data) in files {
                let name = name.rsplit(['/', '\\']).next().unwrap_or(&name).to_string();
                flux.upload_asset(who, ctx.id, id, &name, &ctype, data).await?;
                n += 1;
            }
            Ok::<usize, crate::core::FluxError>(n)
        }
    };
    match s {
        ["releases"] => {
            let input = ReleaseInput {
                tag: f.trim("tag").into(),
                name: Some(f.trim("name").to_string()).filter(|n| !n.is_empty()),
                body: Some(f.get("body").replace("\r\n", "\n")),
                draft: Some(f.checked("draft")),
                prerelease: Some(f.checked("prerelease")),
                target: Some(f.trim("target").to_string()).filter(|t| !t.is_empty()),
            };
            let r = match flux.create_release(who, ctx.id, input).await {
                Ok(r) => r,
                Err(e) => return done(&format!("{b}/releases/new"), Err::<String, _>(e)),
            };
            if let Err(e) = upload(r.id, &f).await {
                return done(&format!("{b}/releases/{}/edit", r.id), Err::<String, _>(e));
            }
            let msg = if r.draft { format!("Saved draft {}", r.tag) } else { format!("Published {}", r.tag) };
            done(&list, Ok::<_, WebError>(msg))
        }
        ["releases", id] => {
            let id = parse_num(id)?;
            let input = ReleaseInput {
                name: Some(f.trim("name").to_string()),
                body: Some(f.get("body").replace("\r\n", "\n")),
                draft: Some(f.checked("draft")),
                prerelease: Some(f.checked("prerelease")),
                ..Default::default()
            };
            match flux.update_release(who, ctx.id, id, input).await {
                Ok(r) => done(&list, Ok::<_, WebError>(format!("Saved {}", r.tag))),
                Err(e) => done(&format!("{b}/releases/{id}/edit"), Err::<String, _>(e)),
            }
        }
        ["releases", id, "delete"] => {
            let id = parse_num(id)?;
            done(&list, flux.delete_release(who, ctx.id, id).await.map(|_| "Release deleted".to_string()))
        }
        ["releases", id, "assets"] => {
            let id = parse_num(id)?;
            let back = format!("{b}/releases/{id}/edit");
            match upload(id, &f).await {
                Ok(0) => done(&back, Err::<String, _>(WebError::BadRequest("choose a file to upload".into()))),
                Ok(n) => done(&back, Ok::<_, WebError>(format!("Uploaded {n} file{}", if n == 1 { "" } else { "s" }))),
                Err(e) => done(&back, Err::<String, _>(e)),
            }
        }
        ["releases", id, "assets", aid, "delete"] => {
            let (id, aid) = (parse_num(id)?, parse_num(aid)?);
            done(
                &format!("{b}/releases/{id}/edit"),
                flux.delete_asset(who, ctx.id, id, aid).await.map(|_| "Asset deleted".to_string()),
            )
        }
        _ => Err(WebError::NotFound("no such action".into())),
    }
}
