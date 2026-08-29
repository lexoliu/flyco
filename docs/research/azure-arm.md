# Azure Spot VM Driver — Implementation Reference

> ## CORRECTION (2026-08-28, verified by live deployment)
>
> Two conclusions below were **wrong**, found by actually deploying against the
> subscription rather than inferring. They are struck here rather than edited in
> place so the reasoning error stays visible.
>
> **1. Spot IS available.** Section 0's "this subscription probably cannot run
> Spot at all" is false. A `Standard_D2als_v6` with `priority: Spot`,
> `evictionPolicy: Deallocate`, `maxPrice: -1` was accepted and provisioned in
> `northcentralus`, then torn down. A subscription lacking spot entitlement is
> rejected at request time with `AzureSpotFeatureNotEnabledForSubscription`;
> that did not occur. The Azure-for-Students-offer argument was documentation
> inference, and the `lowPriorityCores: 3` quota — treated below as a possible
> phantom — is real. Keep the spot-then-fall-back path as defensive design;
> B-series really is spot-ineligible.
>
> **2. A subscription-level Azure Policy restricts regions, and this document
> misses it entirely — which invalidates every regional recommendation below,
> including "westus2 (best)".** Policy *Allowed resource deployment regions*
> (`b86dabb9-b578-4d7b-b842-3b45e95769a1`) sets
> `listOfAllowedLocations = [norwayeast, mexicocentral, northcentralus,
> westus3, canadacentral]`. Deploying to westus2 fails validation with
> `RequestDisallowedByAzure` for *every* resource, the vnet included.
>
> **`az vm list-skus` does not reflect this policy** — it reports westus2 SKUs
> as unrestricted. SKU restrictions and deployment policy are independent
> gates, and §5's availability tables measure only the former. A catalog must
> intersect three things: SKU restrictions, quota, and policy-allowed regions.
> Read the policy from the subscription's assignments rather than hardcoding a
> list; absence of such an assignment means all regions, not none.
>
> Verified unrestricted small D-series in allowed regions: `northcentralus` —
> `D2ads_v5`, `D2ads_v6`, `D2alds_v6`, `D2als_v6`; `canadacentral` — ARM64
> `D2pds_v5`, `D2plds_v5`, `D2pls_v5`, `D2ps_v5`.
>
> Observation, not a conclusion: that spot VM sat in `Creating` for over five
> minutes, well beyond the 45–90s §7 estimates. Do not assume a 60s provision.
>
> Everything else — auth, call sequence, request bodies, async polling,
> Scheduled Events, retail prices — was not contradicted by the test.



Legend: **[E]** = verified empirically on this Mac against subscription
`e47d07d8-2715-4909-aa56-1bfde801bdf0` ("Azure for Students", tenant rit.edu) via read-only
`az` / `az rest` GET. **[D]** = asserted from Microsoft Learn docs. **[I]** = inference.

Nothing was deployed. Deployability is inferred from SKU restrictions + quota, never proven by a
live `PUT` (no `deployments/validate` POST either — that would violate the GET-only constraint).

---

## 0. Headline: this subscription probably cannot run Spot at all

Three independent facts collide:

1. **[D]** Microsoft's spot doc lists the supported offer types: Enterprise Agreement,
   Pay-as-you-go (003P), Sponsored (0036P/0136P), CSP. **Azure for Students is not on the list.**
2. **[E]** `subscriptionPolicies.quotaId` = `AzureForStudents_2018-01-01`, `spendingLimit: On`,
   `promotions[0].category: freetier` (ends 2027-08-24).
3. **[E]** `lowPriorityCores` quota is **3** (not 0) in every region checked — which *suggests*
   spot might work, contradicting (1).

The quota row existing is not proof of entitlement; the error to expect is
`AzureSpotFeatureNotEnabledForSubscription` **[D]**. Treat spot as an attempt-and-fall-back path.

And separately: **[D] B-series (Bsv2 / Basv2 / Bpsv2 included) is explicitly unsupported for
Spot** — yet **[E]** `list-skus` reports `LowPriorityCapable: True` for every B SKU and the retail
prices API returns `... Spot` meters for them. Both catalogs lie. Error code is
`AzureSpotIsNotSupportedForThisVMSize` **[D]**.

Since B-series is the *only* family with non-zero quota on this subscription (§6), the intersection
of {SKU available} ∩ {family quota > 0} ∩ {spot supported} is **empty**. See §5.

---

## 1. Auth — client credentials against Microsoft Entra ID

### 1.1 Token endpoint

```
POST https://login.microsoftonline.com/{tenant_id}/oauth2/v2.0/token
Content-Type: application/x-www-form-urlencoded
```

Form body (exactly four fields) **[D]**:

| field | value |
|---|---|
| `grant_type` | `client_credentials` |
| `client_id` | the app registration's Application (client) ID |
| `client_secret` | the SP secret, URL-encoded |
| `scope` | `https://management.azure.com/.default` |

`scope` is the resource identifier URI + `/.default` — **not** a space-separated permission list.
The `.default` form is mandatory for client credentials. For this subscription
`tenant_id` = `f9dd8f4f-3b8b-4768-aba7-bbd379e0736b` **[E]**.

Response **[D]**:

```json
{
  "token_type": "Bearer",
  "expires_in": 3599,
  "ext_expires_in": 3599,
  "access_token": "eyJ0eXAiOiJKV1QiLCJhbG..."
}
```

- No `refresh_token` is issued in this flow — you re-request with the secret **[D]**.
- Lifetime is ~3600s. **[I]** Cache the token keyed by `(tenant, client_id, scope)` and refresh at
  ~80% of `expires_in` (or on any 401 with `WWW-Authenticate: Bearer ... invalid_token`). On
  wasm32 use a monotonic clock captured at issue time, not the JWT `exp` — you should not need a
  JWT parser in the driver at all.
- Every ARM call then carries `Authorization: Bearer <token>` and `Content-Type: application/json`.

Docs: https://learn.microsoft.com/en-us/entra/identity-platform/v2-oauth2-client-creds-grant-flow ,
https://learn.microsoft.com/en-us/entra/identity-platform/scopes-oidc

### 1.2 Minting the SP — **blocked on this tenant** [E]

```
az rest --method get --url "https://graph.microsoft.com/v1.0/policies/authorizationPolicy"
  → defaultUserRolePermissions.allowedToCreateApps = false
```

**[E]** This is `false` in *both* of the user's tenants (rit.edu `f9dd8f4f-…` **and** the second
tenant `4e8c276c-…` behind "Azure subscription 1"). **[E]** `GET /v1.0/me/memberOf` returns only
groups — no directory role (no Global Administrator, no Application Developer). **[E]**
`GET /v1.0/me/ownedObjects` returns 0 objects.

⇒ `az ad sp create-for-rbac` will fail with `Authorization_RequestDenied` for this user. This is a
tenant policy, not a subscription limit, and only a tenant admin (or being granted the
**Application Developer** directory role) unblocks it. **This invalidates the client-credentials
design as-is on this account.** Practical alternatives, in order of preference:

1. Ask the rit.edu tenant admin for the **Application Developer** role (least privilege that
   allows self-service app registration).
2. Use a **user-assigned managed identity** instead of an SP — a `Microsoft.ManagedIdentity`
   resource is an ARM resource, needs no Graph app-registration rights, and the driver keeps the
   same `Bearer` model. It only works when the driver runs *on Azure*, which a wasm32 client is
   not — so this is only viable if you add a broker.
3. Personal tenant: create a new Entra tenant on a personal (non-rit.edu) account where you are
   Global Admin, and move/attach a subscription. **[E]** confirms the user's second tenant is
   *also* locked, so this must be a genuinely new tenant.

### 1.3 The command (for when app creation is unblocked)

```bash
az ad sp create-for-rbac \
  --name flyco-driver \
  --role Contributor \
  --scopes /subscriptions/e47d07d8-2715-4909-aa56-1bfde801bdf0/resourceGroups/flyco-rg
```

Output gives `appId` (→ `client_id`), `password` (→ `client_secret`), `tenant` (→ `tenant_id`).
Default secret lifetime is 1 year; add `--years N` to change it.

### 1.4 Which role? — answered from the live role definitions [E]

`az role definition list --name "Virtual Machine Contributor"` shows it holds:

```
Microsoft.Network/networkInterfaces/*            ← can create NICs
Microsoft.Network/publicIPAddresses/read
Microsoft.Network/publicIPAddresses/join/action  ← can attach, CANNOT create
Microsoft.Network/virtualNetworks/read
Microsoft.Network/virtualNetworks/subnets/join/action ← can attach, CANNOT create
Microsoft.Network/networkSecurityGroups/read + join/action ← CANNOT create
Microsoft.Resources/subscriptions/resourceGroups/read ← CANNOT create the RG
```

So **Virtual Machine Contributor alone is insufficient** for a driver that provisions its own
VNet / public IP / NSG. Two workable shapes:

- **`Contributor` scoped to one resource group** — simplest, and `notActions` still blocks all
  `Microsoft.Authorization/*/Write`, so the SP cannot grant itself more **[E]**. Recommended.
- **`Virtual Machine Contributor` + `Network Contributor`**, both scoped to the same RG.
  `Network Contributor` = `Microsoft.Network/*` **[E]**. Use this if you want the SP unable to
  touch storage/keyvault/etc.

**Important [E/D]:** *no* RG-scoped role can create the resource group itself — that write happens
at subscription scope. **Create the RG once, out of band**, and the driver's steady-state call
sequence has no RG `PUT` in it. Corollary **[D]**: async-operation status URLs are not scoped to
the resource, so the SP needs its permission at **resource-group** level, not resource level, or it
can start operations but not poll them
(https://learn.microsoft.com/en-us/azure/azure-resource-manager/management/async-operations#permission-for-tracking-async-status).

---

## 2. Minimal resource set for one SSH-reachable Linux VM

### 2.1 Does a VM need pre-created network resources?

Yes. ARM has no "create VM and its network in one `PUT`". `Microsoft.Compute/virtualMachines`
references a NIC by resource ID; the NIC references a subnet and a public IP by resource ID. Each
is its own `PUT`. **[D]**

And the public IP forces an NSG: **[D]** Basic SKU public IPs were **retired 30 September 2025**,
so you must use **Standard**, and Standard is
*"Secure by default model and be closed to inbound traffic when used as a frontend. Allow traffic
with network security group (NSG) is required"*
(https://learn.microsoft.com/en-us/azure/virtual-network/ip-services/public-ip-addresses#sku).
Standard also forces `publicIPAllocationMethod: "Static"` **[D]** — which is a feature here,
because the address then survives deallocate/start (§3).

### 2.2 Smallest call sequence

**6 PUTs cold, 3 PUTs per session** (RG, VNet and NSG are shared workspace infrastructure; only
the PIP, NIC and VM are per-session):

| # | Resource | Depends on | Parallel? |
|---|---|---|---|
| 1 | resource group | — | one-time, out of band |
| 2 | virtual network (subnet **inline**, not a separate PUT) | RG | one-time |
| 3 | network security group (SSH rule) | RG | ← these three |
| 4 | public IP address | RG | ← can go in |
| 5 | network interface | subnet + PIP + NSG | parallel |
| 6 | virtual machine | NIC | — |

Steps 3/4 are independent of 2 and of each other; only step 5 joins them and step 6 needs 5. So the
**critical path is 3 round-trips deep** (network fan-out → NIC → VM), not 6.

**1-PUT alternative [D]:** a single `PUT .../providers/Microsoft.Resources/deployments/{name}` with
an inline ARM template containing all five resources. ARM resolves the dependency graph server-side
and you poll one `Azure-AsyncOperation`. Tradeoff: you give up per-resource error attribution and
you must embed a template. For a wasm32 driver with a hand-rolled HTTP client this is genuinely
attractive — one auth'd call, one poll loop.

### 2.3 API versions — pinned from this subscription's live provider manifest [E]

| Resource type | api-version | why |
|---|---|---|
| `Microsoft.Resources/resourceGroups` | `2023-07-01` | latest stable **[E]** |
| `Microsoft.Resources/deployments` | `2025-04-01` | latest stable **[E]** |
| `Microsoft.Network/virtualNetworks` | `2024-05-01` | **[E]** confirmed present |
| `Microsoft.Network/networkSecurityGroups` | `2024-05-01` | same manifest |
| `Microsoft.Network/publicIPAddresses` | `2024-05-01` | same manifest |
| `Microsoft.Network/networkInterfaces` | `2024-05-01` | same manifest |
| `Microsoft.Compute/virtualMachines` | `2024-11-01` | **[E]**; ≥`2019-03-01` needed for Spot, ≥`2021-03-01` for `deleteOption` **[D]** |
| `Microsoft.Compute/disks` | `2025-01-02` | **[E]** |

**[E]** `Microsoft.Compute`, `Microsoft.Network`, `Microsoft.Resources`, `Microsoft.Quota` are all
already `Registered` on this subscription — no `providers/register` call needed.

### 2.4 Full JSON bodies

Substitute: `SUB` = subscription id, `RG` = `flyco-rg`, `LOC` = `westus2`, `N` = session name.

#### (1) Resource group — one-time, needs subscription-scope write

```
PUT https://management.azure.com/subscriptions/{SUB}/resourcegroups/{RG}?api-version=2023-07-01
```
```json
{
  "location": "westus2",
  "tags": { "owner": "flyco", "ephemeral": "true" }
}
```
Synchronous — returns `201 Created` with `properties.provisioningState: "Succeeded"`.
Docs: https://learn.microsoft.com/en-us/rest/api/resources/resource-groups/create-or-update

> **wasm32 caveat [I]:** this design assumes **WASI / server-side wasm**. If the driver ships as
> *browser* wasm, client credentials is not viable at all — `login.microsoftonline.com` does not
> serve the client-credentials grant to browser origins, ARM does not emit permissive CORS for
> arbitrary origins, and a `client_secret` delivered in browser wasm is simply published. A browser
> target needs a token-brokering backend and this whole §1 moves server-side.

#### (2) Virtual network, subnet inline

```
PUT https://management.azure.com/subscriptions/{SUB}/resourceGroups/{RG}/providers/Microsoft.Network/virtualNetworks/flyco-vnet?api-version=2024-05-01
```
```json
{
  "location": "westus2",
  "properties": {
    "addressSpace": { "addressPrefixes": ["10.42.0.0/16"] },
    "subnets": [
      {
        "name": "default",
        "properties": { "addressPrefix": "10.42.0.0/24" }
      }
    ]
  }
}
```
Subnet ID afterwards:
`/subscriptions/{SUB}/resourceGroups/{RG}/providers/Microsoft.Network/virtualNetworks/flyco-vnet/subnets/default`

Docs: https://learn.microsoft.com/en-us/rest/api/virtualnetwork/virtual-networks/create-or-update

#### (3) Network security group — mandatory, Standard PIP is closed by default

```
PUT https://management.azure.com/subscriptions/{SUB}/resourceGroups/{RG}/providers/Microsoft.Network/networkSecurityGroups/flyco-nsg?api-version=2024-05-01
```
```json
{
  "location": "westus2",
  "properties": {
    "securityRules": [
      {
        "name": "allow-ssh-inbound",
        "properties": {
          "protocol": "Tcp",
          "sourcePortRange": "*",
          "destinationPortRange": "22",
          "sourceAddressPrefix": "Internet",
          "destinationAddressPrefix": "*",
          "access": "Allow",
          "priority": 1000,
          "direction": "Inbound"
        }
      }
    ]
  }
}
```
Tighten `sourceAddressPrefix` to the operator's CIDR when known. Docs:
https://learn.microsoft.com/en-us/rest/api/virtualnetwork/network-security-groups/create-or-update

#### (4) Public IP — Standard SKU, Static, non-zonal

```
PUT https://management.azure.com/subscriptions/{SUB}/resourceGroups/{RG}/providers/Microsoft.Network/publicIPAddresses/flyco-{N}-pip?api-version=2024-05-01
```
```json
{
  "location": "westus2",
  "sku": { "name": "Standard", "tier": "Regional" },
  "properties": {
    "publicIPAddressVersion": "IPv4",
    "publicIPAllocationMethod": "Static",
    "idleTimeoutInMinutes": 4,
    "dnsSettings": { "domainNameLabel": "flyco-{N}" }
  }
}
```

Notes: **[D]** Standard requires `Static` (Dynamic is rejected). Omit `zones` entirely — see §5 on
zone restrictions. `dnsSettings.domainNameLabel` gets you
`flyco-{N}.westus2.cloudapp.azure.com`, which is nicer to hand a dev session than a bare IP and
removes the need to read the allocated address back. The label must be unique per region.
Docs: https://learn.microsoft.com/en-us/rest/api/virtualnetwork/public-ip-addresses/create-or-update

#### (5) Network interface — joins subnet + PIP + NSG

```
PUT https://management.azure.com/subscriptions/{SUB}/resourceGroups/{RG}/providers/Microsoft.Network/networkInterfaces/flyco-{N}-nic?api-version=2024-05-01
```
```json
{
  "location": "westus2",
  "properties": {
    "networkSecurityGroup": {
      "id": "/subscriptions/{SUB}/resourceGroups/{RG}/providers/Microsoft.Network/networkSecurityGroups/flyco-nsg"
    },
    "ipConfigurations": [
      {
        "name": "ipconfig1",
        "properties": {
          "primary": true,
          "privateIPAllocationMethod": "Dynamic",
          "subnet": {
            "id": "/subscriptions/{SUB}/resourceGroups/{RG}/providers/Microsoft.Network/virtualNetworks/flyco-vnet/subnets/default"
          },
          "publicIPAddress": {
            "id": "/subscriptions/{SUB}/resourceGroups/{RG}/providers/Microsoft.Network/publicIPAddresses/flyco-{N}-pip",
            "properties": { "deleteOption": "Detach" }
          }
        }
      }
    ]
  }
}
```

`publicIPAddress.properties.deleteOption` controls what happens to the PIP when the **NIC** is
deleted **[D]**. `Detach` keeps the address (and therefore the DNS label) across a full
delete/recreate cycle — the right choice for a resumable dev session. Use `"Delete"` if you want
one-shot cleanup.
Docs: https://learn.microsoft.com/en-us/rest/api/virtualnetwork/network-interfaces/create-or-update

#### (6) The VM — spot, cloud-init, SSH key, disk kept on delete

```
PUT https://management.azure.com/subscriptions/{SUB}/resourceGroups/{RG}/providers/Microsoft.Compute/virtualMachines/flyco-{N}?api-version=2024-11-01
```
```json
{
  "location": "westus2",
  "tags": { "owner": "flyco", "session": "{N}" },
  "properties": {
    "priority": "Spot",
    "evictionPolicy": "Deallocate",
    "billingProfile": { "maxPrice": -1 },
    "hardwareProfile": { "vmSize": "Standard_D2als_v7" },
    "storageProfile": {
      "imageReference": {
        "publisher": "Canonical",
        "offer": "ubuntu-24_04-lts",
        "sku": "server",
        "version": "latest"
      },
      "osDisk": {
        "name": "flyco-{N}-osdisk",
        "createOption": "FromImage",
        "caching": "ReadWrite",
        "diskSizeGB": 30,
        "deleteOption": "Detach",
        "managedDisk": { "storageAccountType": "StandardSSD_LRS" }
      },
      "dataDisks": []
    },
    "osProfile": {
      "computerName": "flyco-{N}",
      "adminUsername": "flyco",
      "customData": "<base64(cloud-config)>",
      "allowExtensionOperations": false,
      "linuxConfiguration": {
        "disablePasswordAuthentication": true,
        "provisionVMAgent": true,
        "ssh": {
          "publicKeys": [
            {
              "path": "/home/flyco/.ssh/authorized_keys",
              "keyData": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA... user@host"
            }
          ]
        }
      }
    },
    "networkProfile": {
      "networkInterfaces": [
        {
          "id": "/subscriptions/{SUB}/resourceGroups/{RG}/providers/Microsoft.Network/networkInterfaces/flyco-{N}-nic",
          "properties": { "primary": true, "deleteOption": "Detach" }
        }
      ]
    },
    "diagnosticsProfile": { "bootDiagnostics": { "enabled": true } }
  }
}
```

Field-by-field:

- **Spot [D]:** `priority: "Spot"` + `evictionPolicy: "Deallocate"` + `billingProfile.maxPrice: -1`.
  `-1` means *never evict for price* — you pay the lesser of current spot price and the standard
  price. `Deallocate` moves the VM to stopped-deallocated on eviction so it can be restarted;
  **[D]** deallocated VMs still count against quota and you still pay for the disks.
  `Delete` would destroy the underlying disks too — wrong for a resumable dev session.
  **[D]** `maxPrice` can only be changed while the VM is deallocated, and the Spot flag can only be
  set **at creation** (no converting an existing VM either direction).
- **`customData` [D]:** base64 of the raw cloud-config text (starting `#cloud-config`), max 64 KB
  *before* encoding. Ubuntu images read it via cloud-init automatically. Prefer `customData` over
  `userData` here: `userData` is *not* consumed by cloud-init, it is only exposed at
  `/metadata/instance/compute/userData` for your own agent to fetch. If you want both, set both.
- **`deleteOption: "Detach"` on the OS disk [D]** is what keeps the disk when the VM is deleted.
  Worth knowing: **[D]** *"By default, disks, NICs, and Public IPs associated with a VM are
  persisted when the VM is deleted"* — omitting `deleteOption` already gives you Detach semantics
  for a standalone VM. Setting it explicitly is still correct: it documents intent and protects you
  against the Flexible-scale-set default, which is `Delete`. Note the CLI/PowerShell wrappers each
  impose their own default; raw REST does not.
- **`diskSizeGB: 30`** — the smallest sane Ubuntu root. `StandardSSD_LRS` keeps idle cost low; use
  `Premium_LRS` only if the chosen SKU is `PremiumIO` and you need the IOPS. (**[E]** every small
  SKU available to this subscription reports `PremiumIO: True`.)
- **Image [E]:** `Canonical / ubuntu-24_04-lts / server` is x64 Gen2. The **arm64** variant is
  `server-arm64`; Gen1 x64 is `server-gen1`. Verified via
  `az vm image list -p Canonical -f ubuntu-24_04-lts --all -l westus2`. 22.04 is
  `Canonical / 0001-com-ubuntu-server-jammy / 22_04-lts-gen2` (arm64: `22_04-lts-arm64`).
  **This matters:** several of the only-deployable SKUs on this subscription are Arm64
  (`Standard_B2pts_v2`, `D2pls_v5`, …) and pairing them with the x64 image SKU fails at deploy.
  Pick the image SKU from the SKU's `CpuArchitectureType` capability, never hardcode.
- `version: "latest"` is accepted by ARM even though `az vm image show --version latest` rejects it.
- `allowExtensionOperations: false` shaves the VM-agent extension handshake off provisioning; drop
  it if you ever want to run a `CustomScript` extension.
- **Do not send `zones`.** See §5.

Docs: https://learn.microsoft.com/en-us/rest/api/compute/virtual-machines/create-or-update ,
https://learn.microsoft.com/en-us/azure/virtual-machines/spot-vms ,
https://learn.microsoft.com/en-us/azure/virtual-machines/delete

---

## 3. Lifecycle operations

All under
`BASE = https://management.azure.com/subscriptions/{SUB}/resourceGroups/{RG}/providers/Microsoft.Compute/virtualMachines/{VM}`,
all with `?api-version=2024-11-01`.

| Operation | Verb + path | Body | Result |
|---|---|---|---|
| Get / poll state | `GET {BASE}?$expand=instanceView` | — | `200`, `properties.instanceView.statuses[]` |
| Deallocate (stop billing compute) | `POST {BASE}/deallocate` | — | `202` + `Azure-AsyncOperation` |
| Power off (keeps compute reserved, still billed) | `POST {BASE}/powerOff?skipShutdown=false` | — | `202` |
| Start | `POST {BASE}/start` | — | `202` |
| Restart | `POST {BASE}/restart` | — | `202` |
| Resize | `PATCH {BASE}` | `{"properties":{"hardwareProfile":{"vmSize":"Standard_D4als_v7"}}}` | `200`/`202` |
| List sizes it can resize to in place | `GET {BASE}/vmSizes` | — | `200` |
| Delete VM (disk/NIC/PIP survive) | `DELETE {BASE}` | — | `202` |
| Force delete VM | `DELETE {BASE}?forceDeletion=true` | — | `202` |
| Delete everything | `DELETE /subscriptions/{SUB}/resourcegroups/{RG}?api-version=2023-07-01` | — | `202` + `Location` |

Docs: https://learn.microsoft.com/en-us/rest/api/compute/virtual-machines

### 3.1 Deallocate → resize → start, and disk survival

**[D]** From the resize doc: *"Deallocating the VM also releases any dynamic IP addresses assigned
to the VM. **The OS and data disks are not affected.**"* So yes — the OS disk survives
deallocate, resize, and start.

The **Static** Standard public IP (§2.4) also survives deallocation, so the SSH endpoint and DNS
label are stable across a stop/start cycle. This is the single best reason to use Standard/Static
rather than fighting the retirement.

Sequencing rules **[D]**:
- Resizing a **running** VM restarts it. Resizing to a size not present on the current hardware
  cluster **requires** deallocation first. For a dev-session driver, always
  `deallocate → PATCH vmSize → start` — it is deterministic and only marginally slower.
- Use `GET {BASE}/vmSizes` to learn what the *current cluster* supports; the full
  `list-skus` set only becomes reachable after deallocation.
- **[D]** Failure mode to guard against: *"If a resize operation fails, the VM model will still
  display the requested size, but the VM will continue running on its previous size."* A `GET`
  after a failed resize is therefore **not** a reliable read of the running size. Trust the async
  operation's terminal status, not the resource body.
- **[D]** Spot VMs resize like on-demand VMs; but changing `billingProfile.maxPrice` requires the
  VM to be deallocated first.

### 3.2 Deleting the VM but keeping the disk + cleaning up the network

With `osDisk.deleteOption: "Detach"` and `networkInterfaces[].properties.deleteOption: "Detach"`,
a plain `DELETE {BASE}` leaves behind the managed disk, the NIC, and the public IP. Then, in order
(the NIC holds references, so it must go first):

```
DELETE .../Microsoft.Network/networkInterfaces/flyco-{N}-nic?api-version=2024-05-01
DELETE .../Microsoft.Network/publicIPAddresses/flyco-{N}-pip?api-version=2024-05-01
```

Deleting the disk (when you finally want to):
```
DELETE .../Microsoft.Compute/disks/flyco-{N}-osdisk?api-version=2025-01-02
```

To recreate a VM on the kept disk, the next `PUT` uses
`osDisk: { "createOption": "Attach", "osType": "Linux", "managedDisk": { "id": "<disk id>" } }`
and **omits `osProfile` entirely** (it is only valid with `createOption: FromImage`). That means
the SSH key and cloud-init are baked in from the first boot only — plan the image accordingly.

**Delete everything:** `DELETE` the resource group. It is a single async call and it removes the
VNet, NSG, NIC, PIP, disks and VM regardless of any `deleteOption`. **[D]** Follow the `Location`
header (RG delete returns `Location`, not `Azure-AsyncOperation`); a `200` on the polling URL means
done, `202` means still running.

### 3.3 Async operation polling — the exact contract

**[D]** https://learn.microsoft.com/en-us/azure/azure-resource-manager/management/async-operations

1. A long-running op returns **`201 Created`** or **`202 Accepted`**. A `200`/`204` means it
   already finished.
2. Inspect headers **in this precedence order**:
   - If **`Azure-AsyncOperation`** is present → use it. This is the authoritative pattern.
   - Else if **`Location`** is present → use that instead.
   - **Never** use `Location` when `Azure-AsyncOperation` is present.
   - `Retry-After` (seconds) tells you how long to wait. If absent, back off yourself
     (**[I]** 1s → 2s → 5s → 10s, capped at 10s, is plenty for VM ops).
3. `GET` the `Azure-AsyncOperation` URL. Body:
   ```json
   {
     "id": "...",
     "name": "{operation-id}",
     "status": "InProgress | Succeeded | Failed | Canceled",
     "startTime": "2017-01-06T20:56:36.002812+00:00",
     "endTime":   "2017-01-06T20:56:56.002812+00:00",
     "percentComplete": 42.0,
     "properties": {},
     "error": { "code": "...", "message": "..." }
   }
   ```
   Terminal set is exactly `{Succeeded, Failed, Canceled}`; **anything else means keep polling**
   (resource providers may return custom in-flight values such as `Accepted` or `Running`). The
   `error` object appears only on `Failed`/`Canceled`.
4. The `Location` pattern is different: you `GET` the URL and read the **HTTP status** —
   `202` = still running, `200` = done and the body holds the final resource.
5. `provisioningState` on the resource body follows the same three-terminal rule; if the field is
   absent, the operation finished successfully.
6. **[D]** Your HTTP client must accept URLs of at least **4 KB** for these two headers — worth
   checking against whatever buffer your wasm32 fetch wrapper uses.
7. **[D]** Polling requires resource-**group**-level permission (§1.4).

**[I]** Design note for the driver: model this as one `poll_async(response) -> Future<Result>` that
branches on the two header patterns once and is reused by every mutating call. Do not special-case
per operation.

---

## 4. In-VM metadata

### 4.1 Scheduled Events — the eviction signal

**[D]** https://learn.microsoft.com/en-us/azure/virtual-machines/linux/scheduled-events

```
GET  http://169.254.169.254/metadata/scheduledevents?api-version=2020-07-01
Header: Metadata: true          (mandatory — omitting it yields 400 Bad Request)
```

`2020-07-01` is the current version and **versions are mandatory** (`{latest}` is dead).

Response:
```json
{
  "DocumentIncarnation": 3,
  "Events": [
    {
      "EventId": "602d9444-d2cd-49c7-8624-8643e7171297",
      "EventType": "Preempt",
      "ResourceType": "VirtualMachine",
      "Resources": ["flyco-abc"],
      "EventStatus": "Scheduled",
      "NotBefore": "Mon, 19 Sep 2016 18:29:47 GMT",
      "Description": "...",
      "EventSource": "Platform",
      "DurationInSeconds": -1
    }
  ]
}
```

Contract points that matter for a driver:

- **`Preempt` = spot eviction, and its minimum notice is 30 SECONDS.** Every other event type gets
  10–15 minutes. **[D]** Delivery is explicitly *"best effort"*.
- **[D]** Recommended polling interval is **once per second**. Combined with the IMDS rate limit of
  5 req/s per VM (§4.2), 1 Hz is the right number — do not poll faster.
- `DocumentIncarnation` increments whenever the `Events` array changes; use it as a cheap change
  token so you only run your handler on real transitions.
- There is **no** `Completed` status. An event's disappearance from the array *is* the completion
  signal. Track the previous array and diff.
- **[D]** The service is enabled lazily: the **first** request can take **up to 2 minutes** and
  events start flowing within 5 minutes. It auto-disables after 24 h of no requests. So the in-VM
  agent must start polling at boot, not on demand.
- **[D]** Events can jump straight to `Started` (hardware failure) or vanish without starting
  (Azure cancelled the maintenance). Handle both.

Acknowledge / expedite:
```
POST http://169.254.169.254/metadata/scheduledevents?api-version=2020-07-01
Header: Metadata: true
Body:   {"StartRequests": [{"EventId": "602d9444-d2cd-49c7-8624-8643e7171297"}]}
```
**[D]** Returns `200` for any valid event id (even if another VM already approved it); `400` means
malformed. Acknowledging applies to **all** resources listed in the event, not just this VM.
**[D]** Events do not proceed until approved *or* `NotBefore` elapses — including user-triggered
restarts, so an unapproving agent adds latency to your own reboots.

**[I]** For an ephemeral dev session the ack is not a "please wait" — for `Preempt` you have ~30s
regardless, so the agent should flush/push state and *then* ack to shorten the window, rather than
holding the event open.

### 4.2 General IMDS

**[D]** https://learn.microsoft.com/en-us/azure/virtual-machines/instance-metadata-service

```
GET http://169.254.169.254/metadata/instance?api-version=2025-04-07
Header: Metadata: true
```

- Latest api-version is **`2025-04-07`**. `GET /metadata/versions` returns the supported set.
- Categories: `/metadata/instance` (compute + network), `/metadata/attested/document`
  (signed proof of VM identity), `/metadata/identity/oauth2/token` (managed identity),
  `/metadata/scheduledevents`, `/metadata/versions`.
- Useful leaves (append `&format=text` for a bare string):
  - `/metadata/instance/compute/vmId`
  - `/metadata/instance/compute/name`, `/resourceGroupName`, `/subscriptionId`, `/location`
  - `/metadata/instance/compute/vmSize`
  - `/metadata/instance/compute/azEnvironment`
  - `/metadata/instance/compute/userData` (base64; **[D]** this is `userData`, not `customData`)
  - `/metadata/instance/network/interface/0/ipv4/ipAddress/0/publicIpAddress`
- **[D] Rate limit: 5 requests/second per VM** (managed-identity category: 20/s, max 5 concurrent).
  Exceeding it returns `429`.
- **[D] Proxies must be bypassed** — *"IMDS is not intended to be used behind a proxy and doing so
  is unsupported… you still must override any default client proxy settings"* even if you believe
  no proxy exists. `curl --noproxy "*"`, `NO_PROXY=169.254.169.254`.
- **[D]** Requests must originate from the primary IP of the primary NIC, and a route for
  `169.254.169.254/32` must exist in the guest routing table.
- `400` = missing `Metadata: true` header or missing `format=json` on a leaf node.

---

## 5. Pricing and catalog

### 5.1 Retail Prices API

Endpoint (unauthenticated, no token needed) **[D]**:
```
https://prices.azure.com/api/retail/prices?api-version=2023-01-01-preview&$filter=<odata>
```

Filterable fields **[D]**: `armRegionName`, `Location`, `meterId`, `meterName`, `productid`,
`skuId`, `productName`, `skuName`, `serviceName`, `serviceId`, `serviceFamily`, `priceType`,
`armSkuName`. Add `currencyCode='EUR'` for non-USD (USD is the billing truth).

Working query for one region **[E]** (this is what produced the table below):
```
serviceName eq 'Virtual Machines'
  and armRegionName eq 'westus2'
  and priceType eq 'Consumption'
```

**Gotchas, all [E]:**
- `2023-01-01-preview` filter values are **case-sensitive** (`'Virtual Machines'` works,
  `'virtual machines'` does not). Earlier versions were not.
- One `armSkuName` returns **five-plus** rows. Discriminate:
  - Windows vs Linux → `productName` contains `"Windows"`. **There is no Linux marker** — Linux is
    the row *without* `Windows`. Filter Windows out.
  - Spot → `skuName` / `meterName` **ends with `" Spot"`**.
  - Low Priority (the older scale-set meter, *not* the same as Spot) → suffix `" Low Priority"`.
  - Dev/Test pricing → `type == "DevTestConsumption"`; exclude via `priceType eq 'Consumption'`.
- Pagination: 1000 records max, follow `NextPageLink` until null. **[E]** A single-region VM query
  spans several pages.
- **[E]** Spot rows carry a short `effectiveStartDate`/`effectiveEndDate` window (e.g.
  `2026-08-01` → `2026-08-31`) — spot prices are republished monthly, so **cache with a TTL, and
  re-read at month boundaries**. On-demand rows have open-ended dates going back years.
- **[D]** Historical spot prices (90 d) and **eviction rates** (28 d) are *not* in this API — they
  live in Azure Resource Graph, table `SpotResources`, types
  `microsoft.compute/skuspotpricehistory/ostype/location` and
  `microsoft.compute/skuspotevictionrate/location`. Worth wiring up: eviction rate is the number
  that actually predicts session survival.

Docs: https://learn.microsoft.com/en-us/rest/api/cost-management/retail-prices/azure-retail-prices

### 5.2 Live prices for the candidate SKUs [E] — USD/hour, Linux, Consumption

| SKU | region | on-demand | spot | spot discount |
|---|---|---:|---:|---|
| `Standard_B2pts_v2` (Arm64, 2 vCPU/1 GB) | westus2 | **0.0084** | 0.00756 | 10% |
| `Standard_B2ats_v2` (x64, 2/1) | westus2 | **0.0094** | 0.00846 | 10% |
| `Standard_B2pls_v2` (Arm64, 2/4) | westus2 | 0.0336 | 0.03024 | 10% |
| `Standard_B2ls_v2` (x64, 2/4) | westus2 | 0.0508 | 0.03744 | 26% |
| `Standard_B2ps_v2` (Arm64, 2/8) | westus2 | 0.0672 | 0.06048 | 10% |
| `Standard_B2s_v2` (x64, 2/8) | westus2 | 0.0924 | 0.07488 | 19% |
| `Standard_D2als_v7` (x64, 2/4) | westus2 | 0.0804 | **0.014858** | **82%** |
| `Standard_D2ls_v7` (x64, 2/4) | westus2 | 0.117 | 0.021622 | 82% |
| `Standard_F2als_v7` (x64, 2/4) | westus2 | 0.121 | 0.022361 | 82% |
| `Standard_B2pts_v2` | centralus | 0.0187 | 0.007528 | 60% |
| `Standard_D2als_v7` | centralus | 0.0804 | 0.014834 | 82% |
| `Standard_B2pts_v2` | canadacentral | 0.0092 | 0.00828 | 10% |
| `Standard_D2als_v7` | eastus | 0.0804 | 0.015598 | 81% |

**[I]** Two conclusions. (a) Spot is only a real discount on D/F-series (~82%); B-series "spot"
meters exist but discount ~10–25% — and B-series cannot be deployed as spot anyway (§0). (b) The
**cheapest reachable machine on this subscription is `Standard_B2pts_v2` on-demand at
$0.0084/h in westus2** — *cheaper than the best spot price of any D/F SKU* ($0.0149/h). For this
subscription, spot is not the cost lever; picking the burstable Arm64 SKU is.

### 5.3 Enumerating SKUs available to THIS subscription + region

Two equivalent routes:

```bash
# CLI — NOTE: by default this HIDES restricted SKUs
az vm list-skus --location westus2 --resource-type virtualMachines -o json
az vm list-skus --location westus2 --resource-type virtualMachines --all   # includes restricted
```
```
GET https://management.azure.com/subscriptions/{SUB}/providers/Microsoft.Compute/skus
      ?api-version=2021-07-01&$filter=location eq 'westus2'
```

**[E]** These disagree and the difference is the whole story: in `eastus` the REST endpoint returned
**1366** `virtualMachines` SKUs while the CLI returned **532**. The CLI silently drops everything
carrying a `Location`-type restriction. **The driver must use the REST endpoint and read
`restrictions` itself** — the convenient view hides exactly the information you need.

Restriction shape **[E]**:
```json
"restrictions": [
  { "type": "Location", "reasonCode": "NotAvailableForSubscription",
    "restrictionInfo": { "locations": ["eastus"] }, "values": ["eastus"] },
  { "type": "Zone", "reasonCode": "NotAvailableForSubscription",
    "restrictionInfo": { "locations": ["eastus"], "zones": ["1","2","3"] }, "values": ["eastus"] }
]
```

Decision rule **[I]**, derived from the data:
- `type: "Location"` present ⇒ **unusable in that region, full stop.**
- `type: "Zone"` present but no `Location` ⇒ **usable, but only as a regional (non-zonal)
  deployment.** Omit `zones` from the PIP and the VM. This is the common case: **[E]** in westus2
  every x64 B SKU (`B2ats_v2`, `B2ts_v2`, `B2als_v2`, `B2ls_v2`, `B2s_v2`) is zone-restricted in
  all three zones but *not* location-restricted; in westeurope essentially every small SKU is.
  A driver that passes `zones: ["1"]` by habit fails on almost everything this subscription can run.
- Neither ⇒ unrestricted.
- Also read `capabilities`: `vCPUs`, `MemoryGB`, `CpuArchitectureType` (**x64 / Arm64** — drives
  image SKU choice), `PremiumIO`, `HyperVGenerations`. Ignore `LowPriorityCapable` — **[E/D]** it
  reports `True` for B-series, which cannot run as Spot.

### 5.4 Regional availability, measured [E]

VM SKUs *not* location-restricted, per region:

| region | total VM SKUs | available | location-restricted |
|---|---:|---:|---:|
| westus2 | 1327 | **911** | 416 |
| westeurope | 1314 | 800 | 514 |
| eastus2 | 1273 | 652 | 621 |
| centralus | 1304 | 623 | 681 |
| eastus | 1366 | 532 | 834 |
| southeastasia | 1271 | 210 | 1061 |
| westus3 | 1274 | 184 | 1090 |
| canadacentral | 1053 | 183 | 870 |
| northeurope | 1298 | 140 | 1158 |
| **uksouth** | 1293 | **0** | 1293 |

**[E] `uksouth` is 100% restricted for this subscription — zero deployable VM SKUs of any size.**
Do not let a region picker choose it.

### 5.5 B-series specifically [E]

- **`eastus` and `eastus2`: every single `Standard_B*` SKU is `Location` /
  `NotAvailableForSubscription`.** All 41 of them. B-series simply cannot be created there on this
  subscription — which is notable because eastus is the default everyone reaches for.
- **`westus2`:** B-series available. x64 (`B2ats_v2`, `B2ts_v2`, `B2ls_v2`, `B2s_v2`, `B2als_v2`)
  carry a **Zone** restriction → deploy non-zonal. Arm64 (`B2pts_v2`, `B2pls_v2`, `B2ps_v2`)
  are **fully unrestricted**.
- **`centralus`, `canadacentral`:** only the **Arm64 `Bpsv2`** family is available, unrestricted.
- **`westeurope`:** x64 `Bsv2` available, zone-restricted.
- **`northeurope`, `southeastasia`, `westus3`, `uksouth`:** no B-series at all.

---

## 6. Quotas

### 6.1 How to read them (three read-only routes, all [E] working here)

```bash
az vm list-usage --location westus2 -o json
```
```
GET https://management.azure.com/subscriptions/{SUB}/providers/Microsoft.Compute/locations/{loc}/usages?api-version=2024-11-01
```
```
GET https://management.azure.com/subscriptions/{SUB}/providers/Microsoft.Compute/locations/{loc}/providers/Microsoft.Quota/quotas?api-version=2025-09-01
```

The first two give `{name:{value}, currentValue, limit}`. The `Microsoft.Quota` route additionally
returns `properties.isQuotaApplicable` and a structured `limit` object, and is the one to use if you
ever want to *request* an increase (`PUT` on `.../quotas/{name}`, and
`.../quotaRequests` to track it). **[E]** `Microsoft.Quota` is already `Registered` here.

The names to read: `cores` (Total Regional vCPUs), `lowPriorityCores` (Total Regional Spot vCPUs),
and one `standard*Family` entry per VM family. **Map a SKU to its quota key via the `family` field
returned by the SKUs API** (e.g. `Standard_B2pts_v2` → `standardBpsv2Family`) — **[E]** those
strings match the usage `name.value` exactly. Do not try to parse the family out of the SKU name.

### 6.2 What this subscription actually has [E]

Identical in every region measured (eastus, eastus2, westus2, westus3, centralus, canadacentral,
westeurope, northeurope, southeastasia, uksouth):

| quota | limit | current |
|---|---:|---:|
| `cores` (Total Regional vCPUs) | **6** | 0 |
| `lowPriorityCores` (Total Regional Spot vCPUs) | **3** | 0 |
| `virtualMachines` | 25000 | 0 |
| `standardBsv2Family` / `standardBasv2Family` / `standardBpsv2Family` | **10** | 0 |
| `standardBSFamily` (original B) | 4 | 0 |
| `standardDSv3Family`, `standardDSv2Family`, `standardEv3Family`, … (legacy v2–v4) | 4 | 0 |
| `standardDCSFamily` | 2 | 0 |
| `dedicatedVCpus` | 0 | 0 |

**[E] Of 232 quota entries, 161 have a limit of 0** — including **every v5/v6/v7 family**:
`StandardDsv7Family`, `StandardDasv7Family`, `StandardDaldsv7Family`, `StandardFalsv7Family`,
`standardDSv5Family`, `standardDCasv6Family`, `standardEadsv7Family`, `StandardDpsv6Family`, …

### 6.3 The cross-reference — the finding that decides the driver [E]

Intersecting {SKU not location-restricted} × {family quota ≥ vCPUs} × {regional `cores` ≥ vCPUs}:

| region | ≤4-vCPU SKUs deployable **on-demand** |
|---|---|
| westus2 | `B2ats_v2`, `B2ts_v2`, `B2als_v2`, `B2ls_v2`, `B2s_v2` (all **zone-restricted** → non-zonal), `B2pts_v2`, `B2pls_v2`, `B2ps_v2` (Arm64, unrestricted) |
| centralus | `B2pts_v2`, `B2pls_v2`, `B2ps_v2` (Arm64 only) |
| canadacentral | `B2pts_v2`, `B2pls_v2`, `B2ps_v2`, `B4pls_v2`, `B4ps_v2` (Arm64 only) |
| westeurope | `B2ts_v2`, `B2ls_v2`, `B2s_v2` (x64, zone-restricted) |
| **eastus / eastus2 / westus3 / northeurope / southeastasia / uksouth** | **NONE** |

Every non-B SKU that is *available* in eastus/eastus2 belongs to a **v6/v7 family whose quota is
0**. Every family with quota is either legacy (v2–v4, not offered any more in these regions) or
B-series (location-restricted in eastus/eastus2).

**⇒ On-demand: the deployable set is exactly the B-series, in westus2 / centralus /
canadacentral / westeurope. eastus and eastus2 are dead for this subscription.**

**⇒ Spot: [I] the deployable set is very likely empty.** Spot bypasses per-family quota and uses
only the regional `lowPriorityCores` pool (**[D]** *"Azure Spot Virtual Machines will have a
separate quota pool"*, and **[E]** the usage list contains no per-family low-priority entries —
this is docs + quota-structure inference, **not** deployment-verified). That would make every
zero-quota D/F v7 SKU spot-deployable at 2 vCPU. But **[D]** B-series is excluded from Spot and
**[D]** Azure for Students is not a supported Spot offer type. If the offer restriction bites,
`lowPriorityCores: 3` is a phantom quota.

**Expect one of:** `AzureSpotFeatureNotEnabledForSubscription` (offer), or
`AzureSpotIsNotSupportedForThisVMSize` (B-series), or `SkuNotAvailable` (capacity),
or `SpotPriceGreaterThanProvidedMaxPrice` / `MaxPriceValueInvalid`, or
`AzureSpotVMNotSupportedInAvailabilitySet`, or `AzureSpotIsNotSupportedForThisAPIVersion` (<2019-03-01).
**[D]** https://github.com/MicrosoftDocs/azure-compute-docs/blob/main/articles/virtual-machines/error-codes-spot.md

Also **[E]**: `spendingLimit: "On"`. When the student credit runs out the subscription is
**disabled**, not merely throttled — every ARM call starts failing. The driver should surface
`ReadOnlyDisabledSubscription` / `DisabledSubscription` as a distinct, non-retryable state.

---

## 7. Driver-design implications

1. **Minimum call sequence: 6 PUTs cold, 3 per session.** RG + VNet + NSG are one-time
   workspace infrastructure; per session you PUT public IP, NIC, VM. The dependency graph is
   only **3 levels deep** — fan out PIP/NSG/VNet concurrently, join at the NIC, then the VM. The
   single-PUT `Microsoft.Resources/deployments` variant is a strong alternative for wasm32: one
   authenticated call, one poll loop, no client-side dependency ordering.

2. **The NSG is not optional.** Basic public IPs died 2025-09-30; Standard is closed to inbound by
   default. A driver that creates VNet + PIP + NIC + VM and skips the NSG produces a VM that
   provisions cleanly and is unreachable on port 22 — the worst kind of failure.

3. **Wall-clock: ~2–4 minutes end to end.** RG/VNet/NSG/PIP each return in single-digit seconds;
   the NIC likewise; the VM `PUT` reaches `Succeeded` in roughly 30–60 s for a 2-vCPU SKU; then
   guest boot + cloud-init before SSH answers. Budget ~45–90 s to `Succeeded` and another
   ~60–90 s to a usable shell. **[I]** — timings are inference, nothing was deployed.

4. **Never send `zones`.** Zone-type restrictions with no Location restriction are the dominant
   pattern for the SKUs this subscription can actually use (all x64 B-series in westus2 and
   westeurope). Regional deployment works; zonal fails. The driver should treat zones as an
   opt-in advanced flag, defaulted off.

5. **Use the SKUs REST endpoint, not `az vm list-skus`.** The CLI hides restricted SKUs by default
   (532 vs 1366 in eastus). The driver's SKU selector must read `restrictions` and classify
   Location vs Zone itself, and must map `family` → quota key to check `limit > 0` before ever
   attempting a deploy. Availability without quota is the default state on this subscription.

6. **Architecture must be data-driven, not hardcoded.** Several of the only-usable SKUs are
   **Arm64** (`B2pts_v2`, `B2pls_v2`, `B2ps_v2`, `D2pls_v5`). Read `CpuArchitectureType` from the
   SKU and select `ubuntu-24_04-lts / server` vs `server-arm64` from it. A hardcoded x64 image
   reference makes centralus and canadacentral entirely unusable.

7. **Student gotcha — eastus is dead.** All 41 `Standard_B*` SKUs are `NotAvailableForSubscription`
   in eastus and eastus2, and every SKU that *is* available there belongs to a family with quota 0.
   `uksouth` is worse: 1293 of 1293 SKUs location-restricted. The viable region list is
   **westus2 (best), centralus, canadacentral, westeurope** — everything else needs a per-region
   probe before it is offered to a user.

8. **Student gotcha — spot is probably unavailable, twice over.** Azure for Students
   (`AzureForStudents_2018-01-01`) is not in the supported Spot offer list, *and* B-series — the
   only family with usable quota — is excluded from Spot regardless of offer. `lowPriorityCores: 3`
   in the quota table is not entitlement. Build the spot path so a `PUT` failure with
   `AzureSpotFeatureNotEnabledForSubscription` or `AzureSpotIsNotSupportedForThisVMSize`
   transparently retries the same body with `priority`/`evictionPolicy`/`billingProfile` removed.

9. **Student gotcha — spot would not even save money here.** `Standard_B2pts_v2` on-demand is
   **$0.0084/h** in westus2, against **$0.0149/h** for the cheapest 82%-off D-series spot. The
   caps are hard anyway: `cores: 6` regional means **at most three** 2-vCPU on-demand VMs
   (one 4-vCPU + one 2-vCPU, etc.), and `lowPriorityCores: 3` means **exactly one** 2-vCPU spot VM
   and never a 4-vCPU one. Design for a single concurrent session, and read
   `currentValue`/`limit` before provisioning so you fail fast with a clear message instead of an
   opaque `QuotaExceeded`.

10. **Blocking issue outside the driver: this account cannot mint the service principal.**
    `allowedToCreateApps` is `false` in *both* of the user's tenants, and the user holds no
    directory role, so `az ad sp create-for-rbac` will be denied. The client-credentials design is
    sound but needs either the **Application Developer** role from an rit.edu tenant admin or a
    fresh personal tenant. Once unblocked, scope **`Contributor`** (or
    `Virtual Machine Contributor` + `Network Contributor`) to one pre-created resource group —
    `Virtual Machine Contributor` alone cannot PUT a VNet, public IP, or NSG, and no RG-scoped role
    can create the RG itself. Grant at RG scope, not resource scope, or the SP can start async
    operations but cannot poll them.
