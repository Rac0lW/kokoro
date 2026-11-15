'use client'

import { Inspector } from '@/components/editor/Inspector'

export function RightSidebar() {
  return (
    <aside className='flex h-full min-h-0 flex-col bg-[var(--surface-panel)]'>
      <Inspector />
    </aside>
  )
}
