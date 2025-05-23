<script lang="ts">
	import {
		codeCompletionSessionEnabled,
		metadataCompletionEnabled,
		stepInputCompletionEnabled,
		usersWorkspaceStore
	} from '$lib/stores'
	import type { TruncatedToken, NewToken } from '$lib/gen'
	import { UserService } from '$lib/gen'
	import { displayDate, copyToClipboard, getLocalSetting, storeLocalSetting } from '$lib/utils'
	import TableCustom from '$lib/components/TableCustom.svelte'
	import { Button } from '$lib/components/common'
	import Drawer from '$lib/components/common/drawer/Drawer.svelte'
	import DrawerContent from '$lib/components/common/drawer/DrawerContent.svelte'
	import { sendUserToast } from '$lib/toast'
	import Tooltip from './Tooltip.svelte'
	import Version from './Version.svelte'
	import { Clipboard, Plus } from 'lucide-svelte'
	import DarkModeToggle from './sidebar/DarkModeToggle.svelte'
	import Toggle from './Toggle.svelte'
	import type { Writable } from 'svelte/store'
	import { createEventDispatcher } from 'svelte'

	export let scopes: string[] | undefined = undefined
	export let newTokenLabel: string | undefined = undefined
	export let newTokenWorkspace: string | undefined = undefined
	export let newToken: string | undefined = undefined

	let newPassword: string | undefined
	let passwordError: string | undefined
	let tokens: TruncatedToken[]
	let newTokenExpiration: number | undefined
	let displayCreateToken = scopes != undefined
	let login_type = 'none'
	let drawer: Drawer

	const dispatch = createEventDispatcher()

	export function openDrawer() {
		loadLoginType()
		listTokens()
		drawer?.openDrawer()
	}

	export function closeDrawer() {
		drawer?.closeDrawer()
		removeHash()
	}

	function removeHash() {
		window.location.hash = ''
	}

	async function setPassword(): Promise<void> {
		if (newPassword) {
			await UserService.setPassword({
				requestBody: {
					password: newPassword
				}
			})
			sendUserToast('Your password was successfully updated')
		} else {
			sendUserToast('Specify a new password value to change your passord', true)
		}
	}

	async function loadLoginType(): Promise<void> {
		login_type = (await UserService.globalWhoami()).login_type
	}

	async function createToken(): Promise<void> {
		newToken = undefined
		let date: Date | undefined
		if (newTokenExpiration) {
			date = new Date()
			date.setDate(date.getDate() + newTokenExpiration)
		}
		newToken = await UserService.createToken({
			requestBody: {
				label: newTokenLabel,
				expiration: date?.toISOString(),
				scopes,
				workspace_id: newTokenWorkspace
			} as NewToken
		})
		dispatch('tokenCreated', newToken)
		listTokens()
		displayCreateToken = false
	}

	let tokenPage = 1
	async function listTokens(): Promise<void> {
		tokens = await UserService.listTokens({ excludeEphemeral: true, page: tokenPage, perPage: 100 })
	}

	async function deleteToken(tokenPrefix: string) {
		await UserService.deleteToken({ tokenPrefix })
		sendUserToast('Succesfully deleted token')
		listTokens()
	}

	function loadSettings() {
		$codeCompletionSessionEnabled =
			(getLocalSetting('codeCompletionSessionEnabled') ?? 'true') == 'true'
		$metadataCompletionEnabled = (getLocalSetting('metadataCompletionEnabled') ?? 'true') == 'true'
		$stepInputCompletionEnabled =
			(getLocalSetting('stepInputCompletionEnabled') ?? 'true') == 'true'
	}

	function updateSetting(store: Writable<boolean>, value: boolean, setting: string) {
		store.set(value)
		storeLocalSetting(setting, value.toString())
	}

	loadSettings()
</script>

<Drawer bind:this={drawer} size="800px" on:close={removeHash}>
	<DrawerContent title="User Settings" on:close={closeDrawer}>
		<div class="flex flex-col h-full">
			<div>
				{#if scopes == undefined}
					<div class="text-xs pt-1 pb-2 text-tertiary flex-col flex">
						Windmill <Version />
					</div>
					<div class="font-semibold flex items-baseline">
						Theme: <DarkModeToggle forcedDarkMode={false} />
					</div>
					<div class="flex flex-wrap md:gap-40 gap-10 mt-2">
						<div>
							<h2 class="border-b">User info</h2>
							<div class="">
								{#if passwordError}
									<div class="text-red-600 text-2xs grow">{passwordError}</div>
								{/if}
								<div class="flex flex-col gap-2 w-full">
									<div class="mt-4">
										<label class="block w-60 mb-2 text-tertiary">
											<div class="text-secondary">email</div>
											<input
												type="text"
												disabled
												value={$usersWorkspaceStore?.email}
												class="input mt-1"
											/>
										</label>
										{#if login_type == 'password'}
											<label class="block w-120">
												<div class="text-secondary">password</div>
												<input
													type="password"
													bind:value={newPassword}
													class="
							w-full
							block
							py-1
							px-2
							rounded-md
							border
							border-gray-300
							shadow-sm
							focus:border-indigo-300 focus:ring focus:ring-indigo-200 focus:ring-opacity-50
							text-sm
							"
												/>
												<Button size="sm" btnClasses="mt-4 w-min" on:click={setPassword}
													>Set password</Button
												>
											</label>
										{:else if login_type == 'github'}
											<span>Authentified through Github OAuth2. Cannot set a password.</span>
										{/if}
									</div>
								</div>
							</div>
						</div>

						<div>
							<h2 class="border-b">AI user settings</h2>

							<div class="flex flex-col gap-4 mt-2">
								<Toggle
									on:change={(e) => {
										updateSetting(
											codeCompletionSessionEnabled,
											e.detail,
											'codeCompletionSessionEnabled'
										)
									}}
									checked={$codeCompletionSessionEnabled}
									options={{
										right: 'Code completion',
										rightTooltip: 'AI completion in the code editors'
									}}
								/>
								<Toggle
									on:change={(e) => {
										updateSetting(metadataCompletionEnabled, e.detail, 'metadataCompletionEnabled')
									}}
									checked={$metadataCompletionEnabled}
									options={{
										right: 'Metadata completion',
										rightTooltip: 'AI completion for summaries and descriptions'
									}}
								/>
								<Toggle
									on:change={(e) => {
										updateSetting(
											stepInputCompletionEnabled,
											e.detail,
											'stepInputCompletionEnabled'
										)
									}}
									checked={$stepInputCompletionEnabled}
									options={{
										right: 'Flow step input completion',
										rightTooltip: 'AI completion for flow step inputs'
									}}
								/>
							</div>
						</div>
					</div>
				{/if}

				<div class="grid grid-cols-2 pt-8 pb-1" class:pt-8={scopes == undefined}>
					<h2 class="py-0 my-0 border-b pt-3">Tokens</h2>
					<div class="flex justify-end border-b pb-1">
						<Button
							size="sm"
							startIcon={{ icon: Plus }}
							btnClasses={displayCreateToken ? 'hidden' : ''}
							on:click={() => {
								displayCreateToken = !displayCreateToken
								newToken = undefined
								newTokenExpiration = undefined
								newTokenLabel = undefined
							}}
						>
							Create token
						</Button>
					</div>
				</div>
				<div class="text-2xs text-tertiary italic pb-6">
					Authentify to the Windmill API with access tokens.
				</div>

				<div>
					<div
						class="{newToken
							? ''
							: 'hidden'} border rounded-md mb-6 px-2 py-2 bg-green-50 dark:bg-green-200 dark:text-green-800 flex flex-row flex-wrap"
					>
						<div>
							Added token: <button
								on:click={() => copyToClipboard(newToken ?? '')}
								class="inline-flex gap-2 items-center"
							>
								{newToken}
								<Clipboard size={12} />
							</button>
						</div>
						<div class="pt-1 text-xs ml-2">
							Make sure to copy your personal access token now. You won’t be able to see it again!
						</div>
					</div>

					<!-- Token creation interface -->
					<div
						class="{displayCreateToken
							? ''
							: 'hidden'} py-3 px-3 border rounded-md mb-6 bg-surface-secondary min-w-min"
					>
						<h3 class="pb-3 font-semibold">Add a new token</h3>
						{#if scopes != undefined}
							{#each scopes as scope}
								<div class="flex flex-col mb-4">
									<label for="label"
										>Scope <Tooltip
											>This token can only be used within its scope. For flows and scripts, it
											allows you to share them without fear of being impersonated. Once a script is
											triggered, the script itself uses a new ephemeral token to be able to act on
											behalf of the token owner.</Tooltip
										></label
									>
									<input disabled type="text" value={scope} />
								</div>
							{/each}
						{/if}
						<div class="flex flex-row flex-wrap gap-x-2 w-full justify-between">
							<div class="flex flex-col">
								<label for="label"
									>Label <span class="text-xs text-tertiary">(optional)</span></label
								>
								<input type="text" bind:value={newTokenLabel} />
							</div>
							<div class="flex flex-col">
								<label for="expires"
									>Expires In &nbsp;<span class="text-xs text-tertiary">(optional)</span>
								</label>
								<select bind:value={newTokenExpiration}>
									<option value={undefined}>No expiration</option>
									<option value={7}>7d</option>
									<option value={30}>30d</option>
									<option value={90}>90d</option>
								</select>
							</div>
							<div class="flex items-end">
								<Button btnClasses="!mt-2" on:click={createToken}>New token</Button>
							</div>
						</div>
					</div>
				</div>
				<div class="overflow-auto">
					<TableCustom>
						<tr slot="header-row">
							<th>prefix</th>
							<th>label</th>
							<th>expiration</th>
							<th>scopes</th>
							<th />
						</tr>
						<tbody slot="body">
							{#if tokens && tokens.length > 0}
								{#each tokens as { token_prefix, expiration, label, scopes }}
									<tr>
										<td class="grow">{token_prefix}****</td>
										<td class="grow">{label ?? ''}</td>
										<td class="grow">{displayDate(expiration ?? '')}</td>
										<td class="grow">{scopes?.join(', ') ?? ''}</td>
										<td class="grow"
											><button
												class="text-red-500 text-xs underline"
												on:click={() => {
													deleteToken(token_prefix)
												}}>Delete</button
											></td
										>
									</tr>
								{/each}
							{:else if tokens && tokens.length === 0}
								<tr class="px-6"
									><td class="text-secondary italic text-xs"> There are no tokens yet</td></tr
								>
							{:else}
								<tr> Loading...</tr>
							{/if}
						</tbody>
					</TableCustom>
					<div class="flex flex-row-reverse gap-2 w-full">
						{#if tokens?.length == 100}
							<button
								class=" p-1 underline text-sm whitespace-nowrap text-center"
								on:click={() => {
									tokenPage += 1
									listTokens()
								}}
								>Next
							</button>
						{/if}
						{#if tokenPage > 1}
							<button
								class="p-1 underline text-sm whitespace-nowrap text-center"
								on:click={() => {
									tokenPage -= 1
									listTokens()
								}}
								>Previous
							</button>
						{/if}
					</div>
				</div>
			</div>
		</div>
	</DrawerContent>
</Drawer>
