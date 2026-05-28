use super::models::QuickstartTemplate;

/// The full curated catalog of stateless quickstart templates.
///
/// Each image must satisfy:
/// - Tag-pinned to a stable version when one is published (avoid `:latest`),
///   so the catalog is reproducible.
/// - Self-contained — no persistent volumes, external databases, or
///   user-supplied secrets to start serving.
///
/// Images that also run as a non-root user and listen on a port `>= 1024`
/// will deploy cleanly on runtimes that enforce the restricted Pod Security
/// Standard. Entries that violate that invariant (root user and/or port
/// `< 1024`) must set `warning` so the deploy modal surfaces the caveat.
pub fn all() -> Vec<QuickstartTemplate> {
    vec![
        QuickstartTemplate {
            id: "welcome".to_string(),
            display_name: "Welcome page".to_string(),
            tagline: "Friendly landing page with pod/host info.".to_string(),
            description: "A tiny Node.js app that serves a welcome page showing \
                the pod name, hostname, and a configurable greeting. Perfect as \
                a first deployment to confirm Rise is wired up end-to-end."
                .to_string(),
            icon_url: "/assets/quickstart/welcome.svg".to_string(),
            image: "paulbouwer/hello-kubernetes:1.10.1".to_string(),
            http_port: 8080,
            learn_more_url: "https://github.com/paulbouwer/hello-kubernetes".to_string(),
            tags: vec!["demo".to_string(), "hello-world".to_string()],
            warning: None,
        },
        QuickstartTemplate {
            id: "whoami".to_string(),
            display_name: "Request echo".to_string(),
            tagline: "Echoes the incoming HTTP request.".to_string(),
            description: "A small Node.js server that pretty-prints request \
                headers, method, path, body, and TLS info as JSON. Useful for \
                inspecting how Rise's ingress, identity tokens, and forwarded \
                headers reach your app."
                .to_string(),
            icon_url: "/assets/quickstart/whoami.svg".to_string(),
            image: "mendhak/http-https-echo:40".to_string(),
            http_port: 8080,
            learn_more_url: "https://github.com/mendhak/docker-http-https-echo".to_string(),
            tags: vec!["debug".to_string(), "diagnostics".to_string()],
            warning: None,
        },
        QuickstartTemplate {
            id: "httpbin".to_string(),
            display_name: "httpbin".to_string(),
            tagline: "HTTP request & response testing service.".to_string(),
            description: "A Go reimplementation of the classic httpbin testing \
                toolkit. Exposes endpoints for status codes, headers, auth \
                schemes, redirects, cookies and more — handy when wiring up \
                clients or debugging proxies."
                .to_string(),
            icon_url: "/assets/quickstart/httpbin.svg".to_string(),
            image: "mccutchen/go-httpbin:2.22.1".to_string(),
            http_port: 8080,
            learn_more_url: "https://github.com/mccutchen/go-httpbin".to_string(),
            tags: vec!["debug".to_string(), "api".to_string()],
            warning: None,
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
            warning: Some(
                "This image runs nginx as root and listens on port 80. It will \
                fail to start on clusters that enforce non-root containers or \
                disallow privileged ports."
                    .to_string(),
            ),
        },
    ]
}
