<script lang="ts" context="module">
	let outTimeout: NodeJS.Timeout | undefined = undefined
</script>

<script lang="ts">
	import { getContext } from 'svelte'
	import { twMerge } from 'tailwind-merge'
	import type { AppEditorContext, AppViewerContext } from '../../types'
	import ComponentHeader from '../ComponentHeader.svelte'
	import type { AppComponent } from './components'

	import AppMultiSelect from '../../components/inputs/AppMultiSelect.svelte'
	import AppMultiSelectV2 from '../../components/inputs/AppMultiSelectV2.svelte'
	import AppModal from '../../components/layout/AppModal.svelte'
	import AppSchemaForm from '../../components/buttons/AppSchemaForm.svelte'
	import AppStepper from '../../components/layout/AppStepper.svelte'
	import AppSelectTab from '../../components/inputs/AppSelectTab.svelte'
	import AppConditionalWrapper from '../../components/layout/AppConditionalWrapper.svelte'
	import AppSelectStep from '../../components/inputs/AppSelectStep.svelte'
	import AppDownload from '../../components/display/AppDownload.svelte'
	import AppLogsComponent from '../../components/display/AppLogsComponent.svelte'
	import AppFlowStatusComponent from '../../components/display/AppFlowStatusComponent.svelte'
	import AppChartJs from '../../components/display/AppChartJs.svelte'
	import AppChartJsV2 from '../../components/display/AppChartJsV2.svelte'
	import AppQuillEditor from '../../components/inputs/AppQuillEditor.svelte'
	import AppList from '../../components/layout/AppList.svelte'
	import AppJobIdLogComponent from '../../components/display/AppJobIdLogComponent.svelte'
	import AppJobIdFlowStatus from '../../components/display/AppJobIdFlowStatus.svelte'
	import AppCarouselList from '../../components/display/AppCarouselList.svelte'
	import AppAccordionList from '../../components/display/AppAccordionList.svelte'
	import AppAggridTableEe from '../../components/display/table/AppAggridTableEe.svelte'
	import AppCustomComponent from '../../components/display/AppCustomComponent.svelte'
	import AppStatCard from '../../components/display/AppStatCard.svelte'
	import AppMenu from '../../components/display/AppMenu.svelte'
	import AppDecisionTree from '../../components/layout/AppDecisionTree.svelte'
	import AppAgCharts from '../../components/display/charts/AppAgCharts.svelte'
	import AppDbExplorer from '../../components/display/dbtable/AppDbExplorer.svelte'
	import AppS3FileInput from '../../components/inputs/AppS3FileInput.svelte'
	import AppAlert from '../../components/display/AppAlert.svelte'
	import AppDateSliderInput from '../../components/inputs/AppDateSliderInput.svelte'
	import AppTimeInput from '../../components/inputs/AppTimeInput.svelte'
	import AppDateTimeInput from '../../components/inputs/AppDateTimeInput.svelte'
	import AppAggridInfiniteTable from '../../components/display/table/AppAggridInfiniteTable.svelte'
	import AppAggridInfiniteTableEe from '../../components/display/table/AppAggridInfiniteTableEe.svelte'
	import AppDisplayComponent from '../../components/display/AppDisplayComponent.svelte'
	import AppTimeseries from '../../components/display/AppTimeseries.svelte'
	import AppHtml from '../../components/display/AppHtml.svelte'
	import AppMarkdown from '../../components/display/AppMarkdown.svelte'
	import VegaLiteHtml from '../../components/display/VegaLiteHtml.svelte'
	import PlotlyHtml from '../../components/display/PlotlyHtml.svelte'
	import PlotlyHtmlV2 from '../../components/display/PlotlyHtmlV2.svelte'
	import AppScatterChart from '../../components/display/AppScatterChart.svelte'
	import AppPieChart from '../../components/display/AppPieChart.svelte'
	import AppTable from '../../components/display/table/AppTable.svelte'
	import AppAggridTable from '../../components/display/table/AppAggridTable.svelte'
	import AppText from '../../components/display/AppText.svelte'
	import AppButton from '../../components/buttons/AppButton.svelte'
	import AppForm from '../../components/buttons/AppForm.svelte'
	import AppFormButton from '../../components/buttons/AppFormButton.svelte'
	import AppCheckbox from '../../components/inputs/AppCheckbox.svelte'
	import AppTextInput from '../../components/inputs/AppTextInput.svelte'
	import AppDateInput from '../../components/inputs/AppDateInput.svelte'
	import AppSelect from '../../components/inputs/AppSelect.svelte'
	import AppBarChart from '../../components/display/AppBarChart.svelte'
	import AppDivider from '../../components/layout/AppDivider.svelte'
	import AppRangeInput from '../../components/inputs/AppRangeInput.svelte'
	import AppTabs from '../../components/layout/AppTabs.svelte'
	import AppContainer from '../../components/layout/AppContainer.svelte'
	import AppSplitpanes from '../../components/layout/AppSplitpanes.svelte'
	import AppIcon from '../../components/display/AppIcon.svelte'
	import AppFileInput from '../../components/inputs/AppFileInput.svelte'
	import AppImage from '../../components/display/AppImage.svelte'
	import AppDrawer from '../../components/layout/AppDrawer.svelte'
	import AppMap from '../../components/display/AppMap.svelte'
	import AppPdf from '../../components/display/AppPdf.svelte'
	import AppCurrencyInput from '../../components/inputs/currency/AppCurrencyInput.svelte'
	import AppSliderInputs from '../../components/inputs/AppSliderInputs.svelte'
	import AppNumberInput from '../../components/inputs/AppNumberInput.svelte'
	import AppNavbar from '../../components/display/AppNavbar.svelte'
	import AppDateSelect from '../../components/inputs/AppDateSelect.svelte'
	import AppDisplayComponentByJobId from '../../components/display/AppRecomputeAll.svelte'
	import AppRecomputeAll from '../../components/display/AppRecomputeAll.svelte'
	import AppUserResource from '../../components/inputs/AppUserResource.svelte'
	import { Anchor } from 'lucide-svelte'
	import { findGridItemParentGrid, isContainer } from '../appUtils'

	export let component: AppComponent
	export let selected: boolean
	export let locked: boolean = false
	export let render: boolean
	export let hidden: boolean
	export let fullHeight: boolean
	export let overlapped: string | undefined = undefined
	export let moveMode: string | undefined = undefined
	export let componentDraggedId: string | undefined = undefined
	const { mode, app, hoverStore, connectingInput } =
		getContext<AppViewerContext>('AppViewerContext')

	const editorContext = getContext<AppEditorContext>('AppEditorContext')
	const componentActive = editorContext?.componentActive

	const movingcomponents = editorContext?.movingcomponents
	$: ismoving =
		movingcomponents != undefined && $mode == 'dnd' && $movingcomponents?.includes(component.id)

	let initializing: boolean | undefined = undefined
	let errorHandledByComponent: boolean = false
	let componentContainerHeight: number = 0
	let componentContainerWidth: number = 0

	let inlineEditorOpened: boolean = false

	function mouseOut() {
		outTimeout && clearTimeout(outTimeout)
		outTimeout = setTimeout(() => {
			if ($hoverStore !== undefined) {
				// In order to avoid flickering when hovering over table actions,
				// we leave the actions to manage the hover state
				if ($hoverStore.startsWith(`${component.id}_`)) {
					return
				}

				$hoverStore = undefined
			}
		}, 50)
	}

	function componentDraggedIsNotChild(componentDraggedId: string, componentId: string) {
		let parentGrid = findGridItemParentGrid($app, componentDraggedId)

		return !parentGrid?.startsWith(`${componentId}-`)
	}

	function areOnTheSameSubgrid(componentDraggedId: string, componentId: string) {
		return (
			findGridItemParentGrid($app, componentDraggedId) === findGridItemParentGrid($app, componentId)
		)
	}

	let cachedComponentDraggedIsNotChild: boolean | undefined
	let cachedAreOnTheSameSubgrid: boolean | undefined

	function updateCache(componentDraggedId: string | undefined) {
		if (componentDraggedId) {
			cachedComponentDraggedIsNotChild = componentDraggedIsNotChild(
				componentDraggedId,
				component.id
			)
			cachedAreOnTheSameSubgrid = areOnTheSameSubgrid(componentDraggedId, component.id)
		} else {
			cachedComponentDraggedIsNotChild = undefined
			cachedAreOnTheSameSubgrid = undefined
		}
	}

	$: updateCache(componentDraggedId)
</script>

<!-- svelte-ignore a11y-mouse-events-have-key-events -->
<!-- svelte-ignore a11y-no-static-element-interactions -->
<div
	on:mouseover|stopPropagation={() => {
		outTimeout && clearTimeout(outTimeout)
		if (component.id !== $hoverStore) {
			$hoverStore = component.id
		}
	}}
	on:mouseout|stopPropagation={mouseOut}
	class={twMerge(
		'h-full flex flex-col w-full component relative',
		initializing ? 'overflow-hidden h-0' : '',
		hidden && $mode === 'preview' ? 'hidden' : ''
	)}
	data-connection-button
>
	{#if locked && componentActive && $componentActive && moveMode === 'move' && componentDraggedId && componentDraggedId !== component.id && cachedAreOnTheSameSubgrid}
		<div
			class={twMerge('absolute inset-0 bg-locked center-center flex-col z-50', 'bg-locked-hover')}
		>
			<div class="bg-surface p-2 shadow-sm rounded-md flex center-center flex-col gap-2">
				<Anchor size={24} class="text-primary " />
				<div class="text-xs"> Anchored: The component cannot be moved. </div>
			</div>
		</div>
	{:else if moveMode === 'insert' && isContainer(component.type) && componentDraggedId && componentDraggedId !== component.id && cachedComponentDraggedIsNotChild}
		<div
			class={twMerge(
				'absolute inset-0  flex-col rounded-md bg-blue-100 dark:bg-gray-800 bg-opacity-50',
				'outline-dashed outline-offset-2 outline-2 outline-blue-300 dark:outline-blue-700',
				overlapped === component?.id ? 'bg-draggedover dark:bg-draggedover-dark' : ''
			)}
		/>
	{/if}
	{#if $mode !== 'preview'}
		<ComponentHeader
			on:mouseover={() => {
				outTimeout && clearTimeout(outTimeout)

				if (component.id !== $hoverStore) {
					$hoverStore = component.id
				}
			}}
			hover={$hoverStore === component.id}
			{component}
			{selected}
			{fullHeight}
			connecting={$connectingInput.opened}
			on:lock
			on:expand
			on:fillHeight
			{locked}
			{inlineEditorOpened}
			hasInlineEditor={component.type === 'textcomponent' &&
				component.componentInput &&
				component.componentInput.type !== 'connected'}
			on:triggerInlineEditor={() => {
				inlineEditorOpened = !inlineEditorOpened
			}}
			{errorHandledByComponent}
			{componentContainerWidth}
		/>
	{/if}

	{#if ismoving}
		<div class="absolute -top-8 w-40">
			<button
				class="border p-0.5 text-xs"
				on:click={() => {
					$movingcomponents = undefined
				}}
			>
				Cancel move
			</button>
		</div>
	{/if}
	<div
		class={twMerge(
			'h-full outline-1',
			$mode === 'dnd' ? 'bg-surface/40' : '',
			$hoverStore === component.id && $mode !== 'preview'
				? $connectingInput.opened
					? 'outline outline-[#f8aa4b]'
					: 'outline outline-blue-400'
				: '',
			selected && $mode !== 'preview' ? 'outline outline-blue-600' : '',
			$mode != 'preview' ? 'cursor-pointer' : '',
			'relative z-auto',
			$app.css?.['app']?.['component']?.class,
			'wm-app-component',
			ismoving ? 'animate-pulse' : ''
		)}
		style={$app.css?.['app']?.['component']?.style}
		bind:clientHeight={componentContainerHeight}
		bind:clientWidth={componentContainerWidth}
	>
		{#if component.type === 'displaycomponent'}
			<AppDisplayComponent
				id={component.id}
				customCss={component.customCss}
				bind:initializing
				componentInput={component.componentInput}
				configuration={component.configuration}
				{render}
			/>
		{:else if component.type === 'logcomponent'}
			<AppLogsComponent />
		{:else if component.type === 'jobidlogcomponent'}
			<AppJobIdLogComponent
				id={component.id}
				customCss={component.customCss}
				bind:initializing
				configuration={component.configuration}
				{render}
			/>
		{:else if component.type === 'flowstatuscomponent'}
			<AppFlowStatusComponent />
		{:else if component.type === 'jobidflowstatuscomponent'}
			<AppJobIdFlowStatus
				id={component.id}
				customCss={component.customCss}
				bind:initializing
				configuration={component.configuration}
				{render}
			/>
		{:else if component.type === 'barchartcomponent'}
			<AppBarChart
				configuration={component.configuration}
				id={component.id}
				customCss={component.customCss}
				bind:initializing
				componentInput={component.componentInput}
				{render}
			/>
		{:else if component.type === 'timeseriescomponent'}
			<AppTimeseries
				id={component.id}
				customCss={component.customCss}
				configuration={component.configuration}
				bind:initializing
				componentInput={component.componentInput}
				{render}
			/>
		{:else if component.type === 'htmlcomponent'}
			<AppHtml
				id={component.id}
				customCss={component.customCss}
				bind:initializing
				componentInput={component.componentInput}
				{render}
			/>
		{:else if component.type === 'customcomponent'}
			<AppCustomComponent
				customComponent={component.customComponent}
				id={component.id}
				componentInput={component.componentInput}
				{render}
			/>
		{:else if component.type === 'mardowncomponent'}
			<AppMarkdown
				id={component.id}
				customCss={component.customCss}
				bind:initializing
				componentInput={component.componentInput}
				configuration={component.configuration}
				{render}
			/>
		{:else if component.type === 'vegalitecomponent'}
			<VegaLiteHtml
				configuration={component.configuration}
				id={component.id}
				bind:initializing
				componentInput={component.componentInput}
				{render}
			/>
		{:else if component.type === 'plotlycomponent'}
			<PlotlyHtml
				id={component.id}
				configuration={component.configuration}
				bind:initializing
				componentInput={component.componentInput}
				{render}
			/>
		{:else if component.type === 'plotlycomponentv2'}
			<PlotlyHtmlV2
				id={component.id}
				configuration={component.configuration}
				bind:initializing
				componentInput={component.componentInput}
				datasets={component.datasets}
				xData={component.xData}
				{render}
			/>
		{:else if component.type === 'scatterchartcomponent'}
			<AppScatterChart
				configuration={component.configuration}
				id={component.id}
				customCss={component.customCss}
				bind:initializing
				componentInput={component.componentInput}
				{render}
			/>
		{:else if component.type === 'piechartcomponent'}
			<AppPieChart
				configuration={component.configuration}
				id={component.id}
				customCss={component.customCss}
				bind:initializing
				componentInput={component.componentInput}
				{render}
			/>
		{:else if component.type === 'agchartscomponent'}
			<AppAgCharts
				configuration={component.configuration}
				id={component.id}
				customCss={component.customCss}
				bind:initializing
				componentInput={component.componentInput}
				datasets={component.datasets}
				xData={component.xData}
				{render}
			/>
		{:else if component.type === 'agchartscomponentee'}
			<AppAgCharts
				configuration={component.configuration}
				id={component.id}
				customCss={component.customCss}
				bind:initializing
				componentInput={component.componentInput}
				datasets={component.datasets}
				xData={component.xData}
				license={component.license}
				ee={true}
				{render}
			/>
		{:else if component.type === 'tablecomponent'}
			<AppTable
				configuration={component.configuration}
				id={component.id}
				customCss={component.customCss}
				bind:initializing
				componentInput={component.componentInput}
				actionButtons={component.actionButtons}
				{render}
			/>
		{:else if component.type === 'dbexplorercomponent'}
			<AppDbExplorer
				configuration={component.configuration}
				id={component.id}
				customCss={component.customCss}
				actions={component.actions ?? []}
				bind:initializing
				{render}
			/>
		{:else if component.type === 'aggridcomponent'}
			<AppAggridTable
				id={component.id}
				configuration={component.configuration}
				bind:initializing
				componentInput={component.componentInput}
				customCss={component.customCss}
				actions={component.actions ?? []}
				actionsOrder={component.actionsOrder ?? undefined}
				{render}
			/>
		{:else if component.type === 'aggridcomponentee'}
			<AppAggridTableEe
				license={component.license}
				id={component.id}
				configuration={component.configuration}
				bind:initializing
				componentInput={component.componentInput}
				customCss={component.customCss}
				actions={component.actions ?? []}
				actionsOrder={component.actionsOrder ?? undefined}
				{render}
			/>
		{:else if component.type === 'aggridinfinitecomponent'}
			<AppAggridInfiniteTable
				id={component.id}
				configuration={component.configuration}
				bind:initializing
				componentInput={component.componentInput}
				customCss={component.customCss}
				actions={component.actions ?? []}
				{render}
			/>
		{:else if component.type === 'aggridinfinitecomponentee'}
			<AppAggridInfiniteTableEe
				license={component.license}
				id={component.id}
				configuration={component.configuration}
				bind:initializing
				componentInput={component.componentInput}
				customCss={component.customCss}
				actions={component.actions ?? []}
				{render}
			/>
		{:else if component.type === 'textcomponent'}
			<AppText
				id={component.id}
				verticalAlignment={component.verticalAlignment}
				horizontalAlignment={component.horizontalAlignment}
				configuration={component.configuration}
				customCss={component.customCss}
				bind:initializing
				bind:editorMode={inlineEditorOpened}
				componentInput={component.componentInput}
				{render}
			/>
		{:else if component.type === 'buttoncomponent'}
			<AppButton
				id={component.id}
				verticalAlignment={component.verticalAlignment}
				horizontalAlignment={component.horizontalAlignment}
				configuration={component.configuration}
				customCss={component.customCss}
				componentInput={component.componentInput}
				recomputeIds={component.recomputeIds}
				bind:errorHandledByComponent
				{render}
			/>
		{:else if component.type === 'downloadcomponent'}
			<AppDownload
				id={component.id}
				verticalAlignment={component.verticalAlignment}
				horizontalAlignment={component.horizontalAlignment}
				configuration={component.configuration}
				customCss={component.customCss}
				{render}
			/>
		{:else if component.type === 'selectcomponent' || component.type === 'resourceselectcomponent'}
			<AppSelect
				recomputeIds={component.recomputeIds}
				id={component.id}
				verticalAlignment={component.verticalAlignment}
				configuration={component.configuration}
				customCss={component.customCss}
				onSelect={component.onSelect}
				{render}
			/>
		{:else if component.type === 'userresourcecomponent'}
			<AppUserResource
				id={component.id}
				verticalAlignment={component.verticalAlignment}
				configuration={component.configuration}
				customCss={component.customCss}
				{render}
			/>
		{:else if component.type === 'multiselectcomponent'}
			<AppMultiSelect
				id={component.id}
				configuration={component.configuration}
				customCss={component.customCss}
				verticalAlignment={component.verticalAlignment}
				{render}
			/>
		{:else if component.type === 'multiselectcomponentv2'}
			<AppMultiSelectV2
				id={component.id}
				configuration={component.configuration}
				customCss={component.customCss}
				verticalAlignment={component.verticalAlignment}
				{render}
			/>
		{:else if component.type === 'formcomponent'}
			<AppForm
				id={component.id}
				horizontalAlignment={component.horizontalAlignment}
				configuration={component.configuration}
				customCss={component.customCss}
				componentInput={component.componentInput}
				recomputeIds={component.recomputeIds}
				bind:errorHandledByComponent
				{render}
			/>
		{:else if component.type === 'formbuttoncomponent'}
			<AppFormButton
				id={component.id}
				verticalAlignment={component.verticalAlignment}
				horizontalAlignment={component.horizontalAlignment}
				configuration={component.configuration}
				customCss={component.customCss}
				componentInput={component.componentInput}
				recomputeIds={component.recomputeIds}
				bind:errorHandledByComponent
				{render}
			/>
		{:else if component.type === 'checkboxcomponent'}
			<AppCheckbox
				id={component.id}
				verticalAlignment={component.verticalAlignment}
				horizontalAlignment={component.horizontalAlignment}
				configuration={component.configuration}
				customCss={component.customCss}
				recomputeIds={component.recomputeIds}
				onToggle={component.onToggle}
				{render}
			/>
		{:else if component.type === 'textinputcomponent'}
			<AppTextInput
				id={component.id}
				verticalAlignment={component.verticalAlignment}
				configuration={component.configuration}
				customCss={component.customCss}
				{render}
			/>
		{:else if component.type === 'quillcomponent'}
			<AppQuillEditor id={component.id} configuration={component.configuration} {render} />
		{:else if component.type === 'textareainputcomponent'}
			<AppTextInput
				id={component.id}
				verticalAlignment={component.verticalAlignment}
				configuration={component.configuration}
				customCss={component.customCss}
				inputType="textarea"
				appCssKey="textareainputcomponent"
				{render}
			/>
		{:else if component.type === 'emailinputcomponent'}
			<AppTextInput
				verticalAlignment={component.verticalAlignment}
				configuration={component.configuration}
				inputType="email"
				appCssKey="emailinputcomponent"
				id={component.id}
				customCss={component.customCss}
				{render}
			/>
		{:else if component.type === 'passwordinputcomponent'}
			<AppTextInput
				verticalAlignment={component.verticalAlignment}
				configuration={component.configuration}
				inputType="password"
				appCssKey="passwordinputcomponent"
				id={component.id}
				customCss={component.customCss}
				{render}
			/>
		{:else if component.type === 'dateinputcomponent'}
			<AppDateInput
				verticalAlignment={component.verticalAlignment}
				configuration={component.configuration}
				inputType="date"
				id={component.id}
				customCss={component.customCss}
				{render}
			/>
		{:else if component.type === 'timeinputcomponent'}
			<AppTimeInput
				verticalAlignment={component.verticalAlignment}
				configuration={component.configuration}
				id={component.id}
				customCss={component.customCss}
				{render}
			/>
		{:else if component.type === 'datetimeinputcomponent'}
			<AppDateTimeInput
				verticalAlignment={component.verticalAlignment}
				configuration={component.configuration}
				inputType="date"
				id={component.id}
				customCss={component.customCss}
				{render}
			/>
		{:else if component.type === 'numberinputcomponent'}
			<AppNumberInput
				verticalAlignment={component.verticalAlignment}
				configuration={component.configuration}
				id={component.id}
				customCss={component.customCss}
				{render}
			/>
		{:else if component.type === 'currencycomponent'}
			<AppCurrencyInput
				verticalAlignment={component.verticalAlignment}
				configuration={component.configuration}
				id={component.id}
				customCss={component.customCss}
				{render}
			/>
		{:else if component.type === 'slidercomponent'}
			<AppSliderInputs
				verticalAlignment={component.verticalAlignment}
				configuration={component.configuration}
				id={component.id}
				customCss={component.customCss}
				{render}
			/>
		{:else if component.type === 'dateslidercomponent'}
			<AppDateSliderInput
				verticalAlignment={component.verticalAlignment}
				configuration={component.configuration}
				id={component.id}
				customCss={component.customCss}
				{render}
			/>
		{:else if component.type === 'horizontaldividercomponent'}
			<AppDivider
				verticalAlignment={component.verticalAlignment}
				horizontalAlignment={component.horizontalAlignment}
				configuration={component.configuration}
				id={component.id}
				customCss={component.customCss}
				position="horizontal"
				{render}
			/>
		{:else if component.type === 'verticaldividercomponent'}
			<AppDivider
				verticalAlignment={component.verticalAlignment}
				horizontalAlignment={component.horizontalAlignment}
				configuration={component.configuration}
				id={component.id}
				customCss={component.customCss}
				position="vertical"
				{render}
			/>
		{:else if component.type === 'rangecomponent'}
			<AppRangeInput
				verticalAlignment={component.verticalAlignment}
				configuration={component.configuration}
				id={component.id}
				customCss={component.customCss}
				{render}
			/>
		{:else if component.type === 'tabscomponent' && component.tabs}
			<AppTabs
				configuration={component.configuration}
				id={component.id}
				tabs={component.tabs}
				disabledTabs={component.disabledTabs}
				onTabChange={component.onTabChange}
				customCss={component.customCss}
				{componentContainerHeight}
				{render}
			/>
		{:else if component.type === 'steppercomponent' && component.tabs}
			<AppStepper
				id={component.id}
				tabs={component.tabs}
				customCss={component.customCss}
				{componentContainerHeight}
				componentInput={component.componentInput}
				onNext={component.onNext}
				onPrevious={component.onPrevious}
				{render}
			/>
		{:else if component.type === 'conditionalwrapper' && component.conditions}
			<AppConditionalWrapper
				id={component.id}
				conditions={component.conditions}
				customCss={component.customCss}
				onTabChange={component.onTabChange}
				{componentContainerHeight}
				{render}
			/>
		{:else if component.type === 'containercomponent'}
			<AppContainer
				groupFields={component.groupFields}
				id={component.id}
				customCss={component.customCss}
				{componentContainerHeight}
				{render}
			/>
		{:else if component.type === 'listcomponent'}
			<AppList
				id={component.id}
				customCss={component.customCss}
				configuration={component.configuration}
				componentInput={component.componentInput}
				{render}
				bind:initializing
			/>
		{:else if component.type === 'verticalsplitpanescomponent'}
			<AppSplitpanes
				id={component.id}
				customCss={component.customCss}
				panes={component.panes}
				{componentContainerHeight}
				{render}
			/>
		{:else if component.type === 'horizontalsplitpanescomponent'}
			<AppSplitpanes
				id={component.id}
				customCss={component.customCss}
				panes={component.panes}
				{componentContainerHeight}
				horizontal={true}
				{render}
			/>
		{:else if component.type === 'iconcomponent'}
			<AppIcon
				verticalAlignment={component.verticalAlignment}
				horizontalAlignment={component.horizontalAlignment}
				configuration={component.configuration}
				id={component.id}
				customCss={component.customCss}
				{render}
			/>
		{:else if component.type === 'fileinputcomponent'}
			<AppFileInput
				configuration={component.configuration}
				id={component.id}
				customCss={component.customCss}
				onFileChange={component.onFileChange}
				{render}
			/>
		{:else if component.type === 's3fileinputcomponent'}
			<AppS3FileInput
				configuration={component.configuration}
				id={component.id}
				customCss={component.customCss}
				onFileChange={component.onFileChange}
				{render}
			/>
		{:else if component.type === 'imagecomponent'}
			<AppImage
				configuration={component.configuration}
				id={component.id}
				customCss={component.customCss}
				{render}
			/>
		{:else if component.type === 'drawercomponent'}
			<AppDrawer
				verticalAlignment={component.verticalAlignment}
				horizontalAlignment={component.horizontalAlignment}
				configuration={component.configuration}
				id={component.id}
				customCss={component.customCss}
				onOpenRecomputeIds={component.onOpenRecomputeIds}
				onCloseRecomputeIds={component.onCloseRecomputeIds}
				{render}
			/>
		{:else if component.type === 'mapcomponent'}
			<AppMap
				configuration={component.configuration}
				id={component.id}
				customCss={component.customCss}
				{render}
			/>
		{:else if component.type === 'pdfcomponent'}
			<AppPdf
				configuration={component.configuration}
				id={component.id}
				customCss={component.customCss}
				{render}
			/>
		{:else if component.type === 'modalcomponent'}
			<AppModal
				verticalAlignment={component.verticalAlignment}
				horizontalAlignment={component.horizontalAlignment}
				configuration={component.configuration}
				id={component.id}
				customCss={component.customCss}
				onOpenRecomputeIds={component.onOpenRecomputeIds}
				onCloseRecomputeIds={component.onCloseRecomputeIds}
				{render}
			/>
		{:else if component.type === 'schemaformcomponent'}
			<AppSchemaForm
				id={component.id}
				componentInput={component.componentInput}
				configuration={component.configuration}
				customCss={component.customCss}
				{initializing}
				{render}
			/>
		{:else if component.type === 'selecttabcomponent'}
			<AppSelectTab
				id={component.id}
				verticalAlignment={component.verticalAlignment}
				horizontalAlignment={component.horizontalAlignment}
				configuration={component.configuration}
				customCss={component.customCss}
				{render}
			/>
		{:else if component.type === 'selectstepcomponent'}
			<AppSelectStep
				id={component.id}
				verticalAlignment={component.verticalAlignment}
				horizontalAlignment={component.horizontalAlignment}
				configuration={component.configuration}
				customCss={component.customCss}
				{render}
			/>
		{:else if component.type === 'chartjscomponent'}
			<AppChartJs
				configuration={component.configuration}
				id={component.id}
				customCss={component.customCss}
				bind:initializing
				componentInput={component.componentInput}
				{render}
			/>
		{:else if component.type === 'chartjscomponentv2'}
			<AppChartJsV2
				configuration={component.configuration}
				id={component.id}
				customCss={component.customCss}
				bind:initializing
				componentInput={component.componentInput}
				datasets={component.datasets}
				xData={component.xData}
				{render}
			/>
		{:else if component.type === 'carousellistcomponent'}
			<AppCarouselList
				id={component.id}
				configuration={component.configuration}
				componentInput={component.componentInput}
				customCss={component.customCss}
				{componentContainerHeight}
				{render}
				bind:initializing
			/>
		{:else if component.type === 'accordionlistcomponent'}
			<AppAccordionList
				id={component.id}
				componentInput={component.componentInput}
				customCss={component.customCss}
				{componentContainerHeight}
				{render}
				bind:initializing
			/>
		{:else if component.type === 'statcomponent'}
			<AppStatCard
				id={component.id}
				configuration={component.configuration}
				customCss={component.customCss}
				{render}
			/>
		{:else if component.type === 'menucomponent'}
			<AppMenu
				id={component.id}
				verticalAlignment={component.verticalAlignment}
				horizontalAlignment={component.horizontalAlignment}
				configuration={component.configuration}
				customCss={component.customCss}
				menuItems={component.menuItems}
				{render}
			/>
		{:else if component.type === 'decisiontreecomponent' && component.nodes}
			<AppDecisionTree
				id={component.id}
				nodes={component.nodes}
				customCss={component.customCss}
				{componentContainerHeight}
				{render}
			/>
		{:else if component.type === 'alertcomponent'}
			<AppAlert
				id={component.id}
				configuration={component.configuration}
				customCss={component.customCss}
				verticalAlignment={component.verticalAlignment}
				{render}
			/>
		{:else if component.type === 'navbarcomponent'}
			<AppNavbar
				id={component.id}
				configuration={component.configuration}
				customCss={component.customCss}
				navbarItems={component.navbarItems}
				{render}
			/>
		{:else if component.type === 'dateselectcomponent'}
			<AppDateSelect
				id={component.id}
				configuration={component.configuration}
				customCss={component.customCss}
				verticalAlignment={component.verticalAlignment}
				{render}
			/>
		{:else if component.type === 'jobiddisplaycomponent'}
			<AppDisplayComponentByJobId
				id={component.id}
				customCss={component.customCss}
				bind:initializing
				configuration={component.configuration}
				{render}
			/>
		{:else if component.type === 'recomputeallcomponent'}
			<AppRecomputeAll
				id={component.id}
				customCss={component.customCss}
				bind:initializing
				configuration={component.configuration}
				horizontalAlignment={component.horizontalAlignment}
				{render}
			/>
		{/if}
	</div>
</div>
{#if initializing}
	<!-- svelte-ignore a11y-mouse-events-have-key-events -->
	<!-- svelte-ignore a11y-no-static-element-interactions -->
	<div
		on:mouseover|stopPropagation={() => {
			if (component.id !== $hoverStore) {
				$hoverStore = component.id
			}
		}}
		on:mouseout|stopPropagation={() => {
			if ($hoverStore !== undefined) {
				$hoverStore = undefined
			}
		}}
		class="absolute inset-0 center-center flex-col bg- border animate-skeleton"
	/>
{/if}
