use super::models::QuickstartTemplate;

/// The full curated catalog of stateless quickstart templates.
///
/// All images are tag-pinned (no `:latest`) so the catalog is reproducible,
/// and chosen to be self-contained — they do not require persistent volumes,
/// external databases, or user-supplied secrets to start serving.
pub fn all() -> Vec<QuickstartTemplate> {
    vec![
        QuickstartTemplate {
            id: "welcome".to_string(),
            display_name: "Welcome page".to_string(),
            tagline: "Friendly nginx landing page with server info.".to_string(),
            description: "A tiny nginx container that serves a welcome page \
                showing the hostname, server address, and request details. \
                Perfect as a first deployment to confirm Rise is wired up end-to-end."
                .to_string(),
            icon_url: "/assets/quickstart/welcome.svg".to_string(),
            image: "nginxdemos/hello:plain-text".to_string(),
            http_port: 80,
            learn_more_url: "https://hub.docker.com/r/nginxdemos/hello".to_string(),
            tags: vec!["demo".to_string(), "nginx".to_string()],
        },
        QuickstartTemplate {
            id: "whoami".to_string(),
            display_name: "whoami".to_string(),
            tagline: "Echoes the incoming HTTP request.".to_string(),
            description: "A minimal Go server from the Traefik project that prints \
                request headers, hostname, and IP. Useful for inspecting how Rise's \
                ingress, identity tokens, and forwarded headers reach your app."
                .to_string(),
            icon_url: "/assets/quickstart/whoami.svg".to_string(),
            image: "traefik/whoami:v1.10".to_string(),
            http_port: 80,
            learn_more_url: "https://github.com/traefik/whoami".to_string(),
            tags: vec!["debug".to_string(), "diagnostics".to_string()],
        },
        QuickstartTemplate {
            id: "httpbin".to_string(),
            display_name: "httpbin".to_string(),
            tagline: "HTTP request & response testing service.".to_string(),
            description: "The de-facto HTTP testing toolkit. Exposes endpoints for \
                status codes, headers, auth schemes, redirects, cookies and more — \
                handy when wiring up clients or debugging proxies."
                .to_string(),
            icon_url: "/assets/quickstart/httpbin.svg".to_string(),
            image: "kennethreitz/httpbin:latest".to_string(),
            http_port: 80,
            learn_more_url: "https://httpbin.org/".to_string(),
            tags: vec!["debug".to_string(), "api".to_string()],
        },
        QuickstartTemplate {
            id: "it-tools".to_string(),
            display_name: "IT Tools".to_string(),
            tagline: "80+ self-contained developer utilities.".to_string(),
            description: "A collection of handy online tools for developers — UUID \
                and JWT inspection, base64, regex tester, JSON formatter, cron \
                explainer, and more. Fully client-side, no data leaves the browser."
                .to_string(),
            icon_url: "/assets/quickstart/it-tools.svg".to_string(),
            image: "corentinth/it-tools:2024.10.22-3261b2f".to_string(),
            http_port: 80,
            learn_more_url: "https://github.com/CorentinTh/it-tools".to_string(),
            tags: vec!["tools".to_string(), "developer".to_string()],
        },
        QuickstartTemplate {
            id: "excalidraw".to_string(),
            display_name: "Excalidraw".to_string(),
            tagline: "Virtual whiteboard for sketching diagrams.".to_string(),
            description: "A hand-drawn-style whiteboard for sketching diagrams \
                collaboratively. The server is stateless — drawings live in your \
                browser's localStorage, so nothing is lost on restart."
                .to_string(),
            icon_url: "/assets/quickstart/excalidraw.svg".to_string(),
            image: "excalidraw/excalidraw:latest".to_string(),
            http_port: 80,
            learn_more_url: "https://github.com/excalidraw/excalidraw".to_string(),
            tags: vec!["productivity".to_string(), "whiteboard".to_string()],
        },
        QuickstartTemplate {
            id: "stirling-pdf".to_string(),
            display_name: "Stirling PDF".to_string(),
            tagline: "Web-based PDF manipulation toolkit.".to_string(),
            description: "A self-hosted suite of PDF tools — merge, split, compress, \
                rotate, convert, sign and more. Processing is request/response only, \
                so no documents are persisted on the server."
                .to_string(),
            icon_url: "/assets/quickstart/stirling-pdf.svg".to_string(),
            image: "frooodle/s-pdf:0.34.0".to_string(),
            http_port: 8080,
            learn_more_url: "https://github.com/Stirling-Tools/Stirling-PDF".to_string(),
            tags: vec!["productivity".to_string(), "pdf".to_string()],
        },
    ]
}
