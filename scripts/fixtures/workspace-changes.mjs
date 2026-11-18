export async function openWorkspaceChanges(page) {
  await page.locator('summary[aria-label="Page actions"]').click();
  await page.getByRole('button', { name: 'Review changes', exact: true }).click();
}
