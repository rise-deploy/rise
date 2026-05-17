---
title: "Custom Domains"
---

# Custom Domains

Rise projects are accessible at their default URL (e.g., `https://my-app.app.example.com`). You can add custom domains to serve your application from your own domain names.

## Adding a Domain

```bash
rise domain add my-app myapp.example.com
```

With a `rise.toml` in your directory:

```bash
rise domain add myapp.example.com
```

## Listing Domains

```bash
rise domain list my-app
```

## Removing a Domain

```bash
rise domain remove my-app myapp.example.com
```

Aliases: `rise domain rm`, `rise domain del`

## DNS and TLS

Any domain can be configured as a custom domain for your project.

> **Note:** Whether DNS resolution and TLS certificates work correctly depends on the Rise deployment configuration. Reach out to your Rise platform administrator to learn more.

As a general pattern, create a CNAME record pointing your domain to the Rise instance:

```
myapp.example.com.  CNAME  rise.example.com.
```

The exact CNAME target depends on your Rise installation.

## Primary Domain

The first custom domain becomes the primary domain, used as the value of the `RISE_APP_URL` environment variable in your deployments. All domains (including the default URL) are included in `RISE_APP_URLS`.

## Domains in rise.toml

Custom domains are managed via the CLI (`rise domain add/remove`). They are not configured through `rise.toml`.
