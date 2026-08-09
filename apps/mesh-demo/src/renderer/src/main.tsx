import { createRoot } from 'react-dom/client';

import '@vibecook/mille-ui/tokens.css';
import '@vibecook/mille-ui/theme/minimal.css';
import './index.css';
import { App } from './App';

const root = document.getElementById('root');
if (root === null) throw new Error('#root not found');
createRoot(root).render(<App />);
