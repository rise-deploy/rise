// @ts-nocheck
// API client for Rise backend

class RiseAPI {
    constructor(baseUrl) {
        this.baseUrl = baseUrl || window.location.origin;
    }

    async request(endpoint, options = {}) {
        const headers = {
            'Content-Type': 'application/json',
            ...options.headers,
        };

        const response = await fetch(`${this.baseUrl}/api/v1${endpoint}`, {
            ...options,
            headers,
            credentials: 'include',  // Always include cookies (rise_jwt)
        });

        if (response.status === 401) {
            // Authentication required - let the app handle showing login page
            throw new Error('Authentication required');
        }

        if (response.status === 403) {
            // Platform access denied - user is authenticated but lacks platform access
            const errorText = await response.text();
            const error = new Error(errorText || 'You do not have access to Rise platform features. Your account is configured for application access only. Please contact your administrator if you need platform access.');
            error.isPlatformAccessDenied = true;
            throw error;
        }

        if (!response.ok) {
            const errorText = await response.text();
            throw new Error(`API error: ${response.status} ${errorText}`);
        }

        // Handle responses with no body (204 No Content, 202 Accepted)
        if (response.status === 204 || response.status === 202) {
            return null;
        }

        return response.json();
    }

    // User endpoints
    async getMe() {
        return this.request('/users/me');
    }

    async lookupUsers(emails) {
        return this.request('/users/lookup', {
            method: 'POST',
            body: JSON.stringify({ emails })
        });
    }

    // Team endpoints
    async getTeams() {
        return this.request('/teams');
    }

    async getTeam(idOrName, params = {}) {
        const queryString = new URLSearchParams(params).toString();
        return this.request(`/teams/${idOrName}${queryString ? '?' + queryString : ''}`);
    }

    async createTeam(name, members, owners) {
        return this.request('/teams', {
            method: 'POST',
            body: JSON.stringify({ name, members, owners })
        });
    }

    async updateTeam(idOrName, updates) {
        return this.request(`/teams/${idOrName}`, {
            method: 'PUT',
            body: JSON.stringify(updates)
        });
    }

    async deleteTeam(idOrName) {
        return this.request(`/teams/${idOrName}`, {
            method: 'DELETE'
        });
    }

    // Project endpoints
    async getProjects() {
        return this.request('/projects');
    }

    async getProject(idOrName, params = {}) {
        const queryString = new URLSearchParams(params).toString();
        return this.request(`/projects/${idOrName}${queryString ? '?' + queryString : ''}`);
    }

    async createProject(name, access_class, owner) {
        return this.request('/projects', {
            method: 'POST',
            body: JSON.stringify({ name, access_class, owner })
        });
    }

    async getAccessClasses() {
        return this.request('/projects/access-classes');
    }

    async updateProject(nameOrId, updates) {
        return this.request(`/projects/${nameOrId}`, {
            method: 'PUT',
            body: JSON.stringify(updates)
        });
    }

    async deleteProject(nameOrId) {
        return this.request(`/projects/${nameOrId}`, {
            method: 'DELETE'
        });
    }

    // Deployment endpoints
    async getProjectDeployments(projectName, params = {}) {
        const queryString = new URLSearchParams(
            Object.entries(params).filter(([_, v]) => v !== null && v !== undefined && v !== '')
        ).toString();
        return this.request(`/projects/${projectName}/deployments${queryString ? '?' + queryString : ''}`);
    }

    async getDeployment(projectName, deploymentId) {
        return this.request(`/projects/${projectName}/deployments/${deploymentId}`);
    }

    async getDeploymentGroups(projectName) {
        return this.request(`/projects/${projectName}/deployment-groups`);
    }

    async stopDeployment(projectName, deploymentId) {
        return this.request(`/projects/${projectName}/deployments/${deploymentId}/stop`, {
            method: 'POST'
        });
    }

    // Create a new deployment from an existing deployment (redeploy/rollback)
    async createDeploymentFrom(projectName, sourceDeploymentId, useSourceEnvVars = false) {
        // Get the source deployment to extract its configuration
        const sourceDeployment = await this.request(`/projects/${projectName}/deployments/${sourceDeploymentId}`);

        return this.request(`/deployments`, {
            method: 'POST',
            body: JSON.stringify({
                project: projectName,
                from_deployment: sourceDeploymentId,
                use_source_env_vars: useSourceEnvVars,
                group: sourceDeployment.deployment_group || 'default',
                http_port: sourceDeployment.controller_metadata?.http_port || 8080,
            })
        });
    }

    // Service account endpoints
    async getProjectServiceAccounts(projectName) {
        return this.request(`/projects/${projectName}/service-accounts`);
    }

    async createServiceAccount(projectName, issuerUrl, claims, allowedEnvironments = null) {
        const body = { issuer_url: issuerUrl, claims };
        if (allowedEnvironments !== null) {
            body.allowed_environments = allowedEnvironments;
        }
        return this.request(`/projects/${projectName}/service-accounts`, {
            method: 'POST',
            body: JSON.stringify(body)
        });
    }

    async updateServiceAccount(projectName, saId, issuerUrl, claims, allowedEnvironments = undefined) {
        const body = { issuer_url: issuerUrl, claims };
        if (allowedEnvironments !== undefined) {
            body.allowed_environments = allowedEnvironments;
        }
        return this.request(`/projects/${projectName}/service-accounts/${saId}`, {
            method: 'PUT',
            body: JSON.stringify(body)
        });
    }

    async deleteServiceAccount(projectName, saId) {
        return this.request(`/projects/${projectName}/service-accounts/${saId}`, {
            method: 'DELETE'
        });
    }

    // Environment endpoints
    async getProjectEnvironments(projectName) {
        return this.request(`/projects/${projectName}/environments`);
    }

    async createEnvironment(projectName, data) {
        return this.request(`/projects/${projectName}/environments`, {
            method: 'POST',
            body: JSON.stringify(data)
        });
    }

    async updateEnvironment(projectName, envName, updates) {
        return this.request(`/projects/${projectName}/environments/${encodeURIComponent(envName)}`, {
            method: 'PATCH',
            body: JSON.stringify(updates)
        });
    }

    async deleteEnvironment(projectName, envName) {
        return this.request(`/projects/${projectName}/environments/${encodeURIComponent(envName)}`, {
            method: 'DELETE'
        });
    }

    // Environment variable endpoints
    async getProjectEnvVars(projectName, environment = null) {
        const params = new URLSearchParams();
        if (environment) params.set('environment', environment);
        const qs = params.toString();
        return this.request(`/projects/${projectName}/env${qs ? '?' + qs : ''}`);
    }

    async getDeploymentEnvVars(projectName, deploymentId) {
        return this.request(`/projects/${projectName}/deployments/${deploymentId}/env`);
    }

    async setEnvVar(projectName, key, value, isSecret, isProtected = true, environment = null) {
        const params = new URLSearchParams();
        if (environment) params.set('environment', environment);
        const qs = params.toString();
        return this.request(`/projects/${projectName}/env/${encodeURIComponent(key)}${qs ? '?' + qs : ''}`, {
            method: 'PUT',
            body: JSON.stringify({ value, is_secret: isSecret, is_protected: isProtected })
        });
    }

    async moveEnvVar(projectName, key, fromEnvironment = null, toEnvironment = null) {
        return this.request(`/projects/${projectName}/env/${encodeURIComponent(key)}/move`, {
            method: 'POST',
            body: JSON.stringify({ from_environment: fromEnvironment, to_environment: toEnvironment })
        });
    }

    async getEnvVarValue(projectName, key, environment = null) {
        const params = new URLSearchParams();
        if (environment) params.set('environment', environment);
        const qs = params.toString();
        return this.request(`/projects/${projectName}/env/${encodeURIComponent(key)}/value${qs ? '?' + qs : ''}`);
    }

    async getDeploymentEnvVarValue(projectName, deploymentId, key) {
        return this.request(
            `/projects/${projectName}/deployments/${deploymentId}/env/${encodeURIComponent(key)}/value`
        );
    }

    async deleteEnvVar(projectName, key, environment = null) {
        const params = new URLSearchParams();
        if (environment) params.set('environment', environment);
        const qs = params.toString();
        return this.request(`/projects/${projectName}/env/${encodeURIComponent(key)}${qs ? '?' + qs : ''}`, {
            method: 'DELETE'
        });
    }

    // Custom domain endpoints
    async getProjectDomains(projectName) {
        return this.request(`/projects/${projectName}/domains`);
    }

    async addCustomDomain(projectName, domain, environment) {
        const body = { domain };
        if (environment) body.environment = environment;
        return this.request(`/projects/${projectName}/domains`, {
            method: 'POST',
            body: JSON.stringify(body)
        });
    }

    async deleteCustomDomain(projectName, domain) {
        return this.request(`/projects/${projectName}/domains/${encodeURIComponent(domain)}`, {
            method: 'DELETE'
        });
    }

    async setCustomDomainPrimary(projectName, domain) {
        return this.request(`/projects/${projectName}/domains/${encodeURIComponent(domain)}/primary`, {
            method: 'PUT'
        });
    }

    async unsetCustomDomainPrimary(projectName, domain) {
        return this.request(`/projects/${projectName}/domains/${encodeURIComponent(domain)}/primary`, {
            method: 'DELETE'
        });
    }

    // Encryption endpoints

    /**
     * Encrypt a plaintext secret for use in extension specs
     * @param {string} plaintext - The plaintext secret to encrypt
     * @returns {Promise<{encrypted: string}>} The encrypted value
     */
    async encryptSecret(plaintext) {
        return this.request('/encrypt', {
            method: 'POST',
            body: JSON.stringify({ plaintext })
        });
    }

    // Extension endpoints

    /**
     * Get all available extension types (globally registered providers)
     */
    async getExtensionTypes() {
        return this.request('/extensions/types');
    }

    /**
     * Get enabled extensions for a project
     */
    async getProjectExtensions(projectName) {
        return this.request(`/projects/${projectName}/extensions`);
    }

    /**
     * Get specific extension details
     */
    async getProjectExtension(projectName, extensionName) {
        return this.request(`/projects/${projectName}/extensions/${extensionName}`);
    }

    /**
     * Enable/create an extension for a project
     * @param {string} extensionType - Extension type identifier (e.g., "aws-rds-provisioner")
     */
    async createExtension(projectName, extensionName, extensionType, spec) {
        return this.request(`/projects/${projectName}/extensions/${extensionName}`, {
            method: 'POST',
            body: JSON.stringify({ extension_type: extensionType, spec })
        });
    }

    /**
     * Update an extension's spec (full replace)
     */
    async updateExtension(projectName, extensionName, spec) {
        return this.request(`/projects/${projectName}/extensions/${extensionName}`, {
            method: 'PUT',
            body: JSON.stringify({ spec })
        });
    }

    /**
     * Patch an extension's spec (merge)
     */
    async patchExtension(projectName, extensionName, spec) {
        return this.request(`/projects/${projectName}/extensions/${extensionName}`, {
            method: 'PATCH',
            body: JSON.stringify({ spec })
        });
    }

    /**
     * Delete an extension
     */
    async deleteExtension(projectName, extensionName) {
        return this.request(`/projects/${projectName}/extensions/${extensionName}`, {
            method: 'DELETE'
        });
    }
}

export const api = new RiseAPI();
