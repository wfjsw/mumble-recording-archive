<script setup lang="ts" generic="T">
// Fixed-row-height virtual list: renders only the rows in view, so lists of
// thousands of channels/speakers stay cheap.
import { computed, onBeforeUnmount, onMounted, ref } from 'vue';

const props = defineProps<{ items: T[]; rowHeight: number; keyOf: (item: T) => string | number }>();
const box = ref<HTMLDivElement>();
const top = ref(0);
const height = ref(300);
const OVERSCAN = 8;

const first = computed(() => Math.max(0, Math.floor(top.value / props.rowHeight) - OVERSCAN));
const last = computed(() => Math.min(props.items.length, Math.ceil((top.value + height.value) / props.rowHeight) + OVERSCAN));
const slice = computed(() => props.items.slice(first.value, last.value));

let ro: ResizeObserver | null = null;
onMounted(() => {
  ro = new ResizeObserver(() => (height.value = box.value!.clientHeight));
  ro.observe(box.value!);
});
onBeforeUnmount(() => ro?.disconnect());

function scrollToIndex(i: number) {
  const el = box.value;
  if (!el) return;
  const y = i * props.rowHeight;
  if (y < el.scrollTop || y + props.rowHeight > el.scrollTop + el.clientHeight) el.scrollTop = y - el.clientHeight / 3;
}
defineExpose({ scrollToIndex });
</script>

<template>
  <div class="vl" ref="box" @scroll="top = ($event.target as HTMLElement).scrollTop">
    <div :style="{ height: `${items.length * rowHeight}px`, position: 'relative' }">
      <div :style="{ position: 'absolute', left: 0, right: 0, top: `${first * rowHeight}px` }">
        <div v-for="item in slice" :key="keyOf(item)" :style="{ height: `${rowHeight}px` }">
          <slot :item="item" />
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.vl {
  overflow-y: auto;
  overflow-x: hidden;
  min-height: 0;
  flex: 1;
}
</style>
