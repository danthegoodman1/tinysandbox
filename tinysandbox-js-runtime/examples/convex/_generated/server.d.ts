export declare function action<Result>(definition: {
  handler: () => Promise<Result>;
}): {
  handler: () => Promise<Result>;
};
