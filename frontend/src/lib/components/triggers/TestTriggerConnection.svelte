<script lang="ts">
	import {
		CancelablePromise,
		KafkaTriggerService,
		NatsTriggerService,
		WebsocketTriggerService
	} from '$lib/gen'
	import { workspaceStore } from '$lib/stores'
	import { sendUserToast } from '$lib/toast'
	import Button from '../common/button/Button.svelte'

	export let kind: 'websocket' | 'nats' | 'kafka'
	export let args: Record<string, any>

	const kindToName: { [key: string]: string } = {
		websocket: 'WebSocket',
		nats: 'NATS server(s)',
		kafka: 'Kafka broker(s)'
	}

	let testLoading: boolean = false
	let promise: CancelablePromise<any> | null = null
	async function testTriggerConnection() {
		if (testLoading) {
			promise?.cancel()
			return
		}

		testLoading = true
		try {
			if (kind === 'websocket') {
				promise = WebsocketTriggerService.testWebsocketConnection({
					workspace: $workspaceStore!,
					requestBody: args as any
				})
			} else if (kind === 'nats') {
				promise = NatsTriggerService.testNatsConnection({
					workspace: $workspaceStore!,
					requestBody: args as any
				})
			} else if (kind === 'kafka') {
				promise = KafkaTriggerService.testKafkaConnection({
					workspace: $workspaceStore!,
					requestBody: args as any
				})
			}
			await promise
			sendUserToast(`Successfully connected to ${kindToName[kind]}`)
		} catch (err) {
			if (!promise?.isCancelled) {
				sendUserToast(`Error testing ${kindToName[kind]}: ${err?.body ?? 'Unknown error'}`, true)
			}
		} finally {
			testLoading = false
		}
	}
</script>

<div class="flex flex-row justify-end mt-1">
	<Button
		spacingSize="sm"
		size="xs"
		color="light"
		variant="border"
		on:click={testTriggerConnection}
		loading={testLoading}
		clickableWhileLoading
	>
		Test connection
	</Button>
</div>
